//! Authorization: what an actor may do on a repo.
//!
//! The Worker entry resolves an actor's role once per request from the
//! DIRECTORY D1 database (shared with the auth worker; see
//! docs/spec-orgs-and-ci.md) and hands it to the repo's Durable Object in the
//! `X-Ripgit-Role` header. The DO is reachable only through the Worker, and
//! the auth worker strips every inbound `X-Ripgit-*` header, so the DO can
//! trust it. Visibility lives in the DO, so the DO applies the public-repo
//! floor itself.

use serde::Deserialize;
use worker::*;

/// Header carrying the resolved role from the Worker to the DO.
pub const ROLE_HEADER: &str = "X-Ripgit-Role";

/// A repo role. Ordered: each role includes everything below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    None,
    Read,
    Triage,
    Write,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::None => "none",
            Role::Read => "read",
            Role::Triage => "triage",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    /// Parse a role name. Anything unknown is no access, never more.
    pub fn parse(s: &str) -> Role {
        match s.trim() {
            "read" => Role::Read,
            "triage" => Role::Triage,
            "write" => Role::Write,
            "admin" => Role::Admin,
            _ => Role::None,
        }
    }

    /// The role the Worker resolved for this request.
    pub fn from_request(req: &Request) -> Role {
        req.headers()
            .get(ROLE_HEADER)
            .ok()
            .flatten()
            .map(|s| Role::parse(&s))
            .unwrap_or(Role::None)
    }
}

/// Who is calling, from the trusted `X-Ripgit-Actor-*` headers.
pub struct Actor {
    /// better-auth user.id. Empty for mirror agents, which are not people.
    pub id: String,
    /// The actor's own namespace (lowercased GitHub login). Used for display
    /// and as the author name on issues and comments.
    pub name: String,
    /// "user" | "agent" | "mirror"
    pub kind: String,
    /// Mirror agents only: the one "owner/repo" they may push to.
    pub repo: Option<String>,
    /// Capabilities granted by the auth worker, e.g. the mirror agent's
    /// `mirror` scope.
    pub scopes: Vec<String>,
}

fn header(req: &Request, name: &str) -> Option<String> {
    req.headers().get(name).ok().flatten()
}

impl Actor {
    pub fn from_request(req: &Request) -> Option<Actor> {
        let name = header(req, "X-Ripgit-Actor-Name")?;
        let scopes = header(req, "X-Ripgit-Actor-Scopes")
            .map(|raw| {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Some(Actor {
            id: header(req, "X-Ripgit-Actor-Id").unwrap_or_default(),
            name,
            kind: header(req, "X-Ripgit-Actor-Kind").unwrap_or_else(|| "user".into()),
            repo: header(req, "X-Ripgit-Actor-Repo"),
            scopes,
        })
    }
}

/// One row answering "what does this actor get on this owner's repos".
#[derive(Deserialize)]
struct RoleRow {
    kind: String,
    user_id: Option<String>,
    org_role: Option<String>,
    default_repo_role: Option<String>,
    grants: Option<String>,
}

// Column names are better-auth's (camelCase, quoted "user"); keep in step with
// auth/migrations/0001_better_auth.sql for the pinned better-auth version.
const ROLE_QUERY: &str = r#"
SELECT n.kind, n.user_id,
       m.role AS org_role,
       s.default_repo_role,
       (SELECT group_concat(g.role) FROM ripgit_repo_grants g
         WHERE g.repo = ?1
           AND ((g.grantee_kind = 'user' AND g.grantee_id = ?2)
             OR (g.grantee_kind = 'team' AND g.grantee_id IN
                  (SELECT tm."teamId" FROM "teamMember" tm WHERE tm."userId" = ?2)))
       ) AS grants
FROM ripgit_namespaces n
LEFT JOIN "member" m ON m."organizationId" = n.org_id AND m."userId" = ?2
LEFT JOIN ripgit_org_settings s ON s.org_id = n.org_id
WHERE n.name = ?3
"#;

/// Resolve `actor`'s role on `owner/repo`, before visibility is applied.
///
/// With `repo: None`, resolves the actor's role over the whole namespace:
/// only namespace-wide roles count (a user's own namespace, org owner/admin,
/// the org default), not grants on individual repos.
///
/// Fails closed: an unclaimed namespace, a missing DIRECTORY binding, or a
/// query error all mean no role.
pub async fn resolve_role(
    env: &Env,
    actor: Option<&Actor>,
    owner: &str,
    repo: Option<&str>,
) -> Role {
    let Some(actor) = actor else {
        return Role::None;
    };
    // "" matches no grant row, so a namespace-level query sees no repo grants.
    let path = repo
        .map(|r| format!("{}/{}", owner, r).to_lowercase())
        .unwrap_or_default();

    // A mirror agent may write to exactly the repo its OIDC grant named.
    if actor.kind == "mirror" {
        return match &actor.repo {
            Some(scope) if !path.is_empty() && scope.to_lowercase() == path => Role::Write,
            _ => Role::None,
        };
    }
    if actor.id.is_empty() {
        return Role::None;
    }

    let db = match env.d1("DIRECTORY") {
        Ok(db) => db,
        Err(e) => {
            console_error!("authz: DIRECTORY binding unavailable: {}", e);
            return Role::None;
        }
    };
    let row = async {
        db.prepare(ROLE_QUERY)
            .bind(&[
                path.clone().into(),
                actor.id.clone().into(),
                owner.to_lowercase().into(),
            ])?
            .first::<RoleRow>(None)
            .await
    }
    .await;

    match row {
        Ok(Some(row)) => role_from_row(&row, &actor.id),
        Ok(None) => Role::None,
        Err(e) => {
            console_error!("authz: role query for {} failed: {}", path, e);
            Role::None
        }
    }
}

/// The highest role any of a comma-separated list grants.
fn max_role(list: Option<&str>) -> Role {
    list.unwrap_or("")
        .split(',')
        .map(Role::parse)
        .max()
        .unwrap_or(Role::None)
}

fn role_from_row(row: &RoleRow, actor_id: &str) -> Role {
    let grants = max_role(row.grants.as_deref());
    match row.kind.as_str() {
        "user" => {
            if row.user_id.as_deref() == Some(actor_id) {
                Role::Admin
            } else {
                grants
            }
        }
        "org" => {
            // better-auth stores several org roles comma-separated.
            let org_roles: Vec<&str> = row
                .org_role
                .as_deref()
                .map(|r| r.split(',').map(str::trim).collect())
                .unwrap_or_default();
            if org_roles.iter().any(|r| *r == "owner" || *r == "admin") {
                Role::Admin
            } else if !org_roles.is_empty() {
                let default = Role::parse(row.default_repo_role.as_deref().unwrap_or("read"));
                default.max(grants)
            } else {
                grants
            }
        }
        _ => Role::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, user_id: Option<&str>, org_role: Option<&str>, default: Option<&str>, grants: Option<&str>) -> RoleRow {
        RoleRow {
            kind: kind.into(),
            user_id: user_id.map(Into::into),
            org_role: org_role.map(Into::into),
            default_repo_role: default.map(Into::into),
            grants: grants.map(Into::into),
        }
    }

    #[test]
    fn roles_are_ordered_and_unknown_names_grant_nothing() {
        assert!(Role::Admin > Role::Write && Role::Write > Role::Triage);
        assert!(Role::Triage > Role::Read && Role::Read > Role::None);
        assert_eq!(Role::parse("owner"), Role::None);
        assert_eq!(Role::parse(" write "), Role::Write);
    }

    #[test]
    fn a_user_namespace_owner_is_admin_and_others_get_only_grants() {
        assert_eq!(role_from_row(&row("user", Some("u1"), None, None, None), "u1"), Role::Admin);
        assert_eq!(role_from_row(&row("user", Some("u1"), None, None, None), "u2"), Role::None);
        assert_eq!(
            role_from_row(&row("user", Some("u1"), None, None, Some("read,write")), "u2"),
            Role::Write
        );
    }

    #[test]
    fn org_roles_map_onto_repo_roles() {
        let owner = row("org", None, Some("owner"), None, None);
        assert_eq!(role_from_row(&owner, "u"), Role::Admin);
        let admin = row("org", None, Some("member,admin"), None, None);
        assert_eq!(role_from_row(&admin, "u"), Role::Admin);
        // Members get the org default unless a grant gives more.
        let member = row("org", None, Some("member"), Some("read"), None);
        assert_eq!(role_from_row(&member, "u"), Role::Read);
        let member_granted = row("org", None, Some("member"), Some("read"), Some("triage,write"));
        assert_eq!(role_from_row(&member_granted, "u"), Role::Write);
        let member_no_default = row("org", None, Some("member"), Some("none"), None);
        assert_eq!(role_from_row(&member_no_default, "u"), Role::None);
        // Missing settings row: members default to read.
        let member_unset = row("org", None, Some("member"), None, None);
        assert_eq!(role_from_row(&member_unset, "u"), Role::Read);
    }

    #[test]
    fn outsiders_of_an_org_get_only_their_grants() {
        assert_eq!(role_from_row(&row("org", None, None, Some("write"), None), "u"), Role::None);
        assert_eq!(role_from_row(&row("org", None, None, None, Some("read")), "u"), Role::Read);
    }
}
