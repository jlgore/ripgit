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

/// Header telling the DO whether the caller belongs to the org that owns the
/// repo ("1" or "0"), which is what `internal` visibility turns on.
pub const MEMBER_HEADER: &str = "X-Ripgit-Org-Member";

/// What the Worker resolved for one request: the caller's role, and whether
/// they are a member of the owning organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub role: Role,
    pub org_member: bool,
}

impl Access {
    pub const NONE: Access = Access {
        role: Role::None,
        org_member: false,
    };

    /// The access the Worker resolved for this request.
    pub fn from_request(req: &Request) -> Access {
        Access {
            role: Role::from_request(req),
            org_member: header(req, MEMBER_HEADER).as_deref() == Some("1"),
        }
    }

    /// Stamp this access onto a request bound for the DO, replacing whatever
    /// arrived under these names.
    pub fn apply(&self, headers: &mut Headers) -> Result<()> {
        headers.set(ROLE_HEADER, self.role.as_str())?;
        headers.set(MEMBER_HEADER, if self.org_member { "1" } else { "0" })
    }
}

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
pub async fn resolve_access(
    env: &Env,
    actor: Option<&Actor>,
    owner: &str,
    repo: Option<&str>,
) -> Access {
    let Some(actor) = actor else {
        return Access::NONE;
    };
    // "" matches no grant row, so a namespace-level query sees no repo grants.
    let path = repo
        .map(|r| format!("{}/{}", owner, r).to_lowercase())
        .unwrap_or_default();

    // A mirror agent may write to exactly the repo its OIDC grant named.
    if actor.kind == "mirror" {
        let role = match &actor.repo {
            Some(scope) if !path.is_empty() && scope.to_lowercase() == path => Role::Write,
            _ => Role::None,
        };
        return Access {
            role,
            org_member: false,
        };
    }
    if actor.id.is_empty() {
        return Access::NONE;
    }

    let db = match env.d1("DIRECTORY") {
        Ok(db) => db,
        Err(e) => {
            console_error!("authz: DIRECTORY binding unavailable: {}", e);
            return Access::NONE;
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
        Ok(Some(row)) => Access {
            role: role_from_row(&row, &actor.id),
            // Only an org namespace has members; a user's own namespace makes
            // them admin, which already reads everything.
            org_member: row.kind == "org" && row.org_role.is_some(),
        },
        Ok(None) => Access::NONE,
        Err(e) => {
            console_error!("authz: role query for {} failed: {}", path, e);
            Access::NONE
        }
    }
}

// ---------------------------------------------------------------------------
// Repo access management (the settings page). ripgit writes only its own
// ripgit_repo_grants table; users, orgs, and teams belong to better-auth.
// ---------------------------------------------------------------------------

/// One grant on a repo, with a label a person can read.
#[derive(Deserialize)]
pub struct GrantView {
    /// "user" | "team"
    pub kind: String,
    pub id: String,
    pub role: String,
    /// The user's login or the team's name.
    pub label: String,
}

#[derive(Deserialize)]
pub struct TeamView {
    pub id: String,
    pub name: String,
}

/// Everything the settings page shows about who can reach a repo.
pub struct RepoAccessInfo {
    /// Whether the owner namespace is an organization (internal visibility and
    /// team grants only make sense there).
    pub is_org: bool,
    pub grants: Vec<GrantView>,
    /// The owning org's teams, to grant from.
    pub teams: Vec<TeamView>,
}

fn directory(env: &Env) -> Result<D1Database> {
    env.d1("DIRECTORY")
}

fn repo_key(owner: &str, repo: &str) -> String {
    format!("{}/{}", owner, repo).to_lowercase()
}

pub async fn load_repo_access(env: &Env, owner: &str, repo: &str) -> Result<RepoAccessInfo> {
    #[derive(Deserialize)]
    struct KindRow {
        kind: String,
    }
    let db = directory(env)?;
    let owner = owner.to_lowercase();

    let is_org = db
        .prepare("SELECT kind FROM ripgit_namespaces WHERE name = ?1")
        .bind(&[owner.clone().into()])?
        .first::<KindRow>(None)
        .await?
        .is_some_and(|row| row.kind == "org");

    let grants = db
        .prepare(
            r#"SELECT g.grantee_kind AS kind, g.grantee_id AS id, g.role,
                      COALESCE(u.login, t.name, g.grantee_id) AS label
               FROM ripgit_repo_grants g
               LEFT JOIN "user" u ON g.grantee_kind = 'user' AND u.id = g.grantee_id
               LEFT JOIN team t ON g.grantee_kind = 'team' AND t.id = g.grantee_id
               WHERE g.repo = ?1
               ORDER BY g.grantee_kind, label"#,
        )
        .bind(&[repo_key(&owner, repo).into()])?
        .all()
        .await?
        .results::<GrantView>()?;

    let teams = if is_org {
        db.prepare(
            r#"SELECT t.id, t.name FROM team t
               JOIN ripgit_namespaces n ON n.org_id = t."organizationId"
               WHERE n.name = ?1
               ORDER BY t.name"#,
        )
        .bind(&[owner.into()])?
        .all()
        .await?
        .results::<TeamView>()?
    } else {
        Vec::new()
    };

    Ok(RepoAccessInfo {
        is_org,
        grants,
        teams,
    })
}

/// Grant `role` on `owner/repo` to a user (by login) or a team (by id, which
/// must belong to the owning org). Replaces any existing grant to the same
/// grantee. The outer error is infrastructure; the inner one is a message for
/// the person who submitted the form.
pub async fn grant(
    env: &Env,
    owner: &str,
    repo: &str,
    kind: &str,
    grantee: &str,
    role: &str,
) -> Result<std::result::Result<(), String>> {
    #[derive(Deserialize)]
    struct IdRow {
        id: String,
    }
    let role = Role::parse(role);
    if role == Role::None {
        return Ok(Err("role must be read, triage, write, or admin".into()));
    }
    let db = directory(env)?;
    let grantee = grantee.trim();

    let id = match kind {
        "user" => db
            .prepare(r#"SELECT id FROM "user" WHERE login = ?1"#)
            .bind(&[grantee.to_lowercase().into()])?
            .first::<IdRow>(None)
            .await?
            .map(|row| row.id)
            .ok_or_else(|| format!("no user with login `{}` has signed in", grantee)),
        "team" => db
            .prepare(
                r#"SELECT t.id FROM team t
                   JOIN ripgit_namespaces n ON n.org_id = t."organizationId"
                   WHERE n.name = ?1 AND t.id = ?2"#,
            )
            .bind(&[owner.to_lowercase().into(), grantee.into()])?
            .first::<IdRow>(None)
            .await?
            .map(|row| row.id)
            .ok_or_else(|| "that team does not belong to this organization".to_string()),
        _ => Err("grantee kind must be user or team".to_string()),
    };
    let id = match id {
        Ok(id) => id,
        Err(message) => return Ok(Err(message)),
    };

    db.prepare(
        "INSERT INTO ripgit_repo_grants (repo, grantee_kind, grantee_id, role) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (repo, grantee_kind, grantee_id) DO UPDATE SET role = excluded.role",
    )
    .bind(&[
        repo_key(owner, repo).into(),
        kind.into(),
        id.into(),
        role.as_str().into(),
    ])?
    .run()
    .await?;
    Ok(Ok(()))
}

pub async fn revoke(env: &Env, owner: &str, repo: &str, kind: &str, id: &str) -> Result<()> {
    directory(env)?
        .prepare(
            "DELETE FROM ripgit_repo_grants WHERE repo = ?1 AND grantee_kind = ?2 AND grantee_id = ?3",
        )
        .bind(&[repo_key(owner, repo).into(), kind.into(), id.into()])?
        .run()
        .await?;
    Ok(())
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
