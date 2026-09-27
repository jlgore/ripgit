-- ripgit's own tables. better-auth never touches these.

-- One row per /:owner/ URL segment, shared by users and orgs. A name is
-- claimed once and never reassigned: repos live in Durable Objects named
-- "{owner}/{repo}" forever.
CREATE TABLE ripgit_namespaces (
  name        TEXT PRIMARY KEY,           -- lowercase URL segment
  kind        TEXT NOT NULL CHECK (kind IN ('user', 'org')),
  user_id     TEXT,                       -- kind='user': "user".id
  org_id      TEXT,                       -- kind='org': organization.id
  created_at  INTEGER NOT NULL,
  CHECK ((kind = 'user') = (user_id IS NOT NULL)),
  CHECK ((kind = 'org') = (org_id IS NOT NULL))
);
CREATE INDEX ripgit_namespaces_user_id_idx ON ripgit_namespaces (user_id);
CREATE INDEX ripgit_namespaces_org_id_idx ON ripgit_namespaces (org_id);

-- Roles granted on a repo, to a team or to a single user.
CREATE TABLE ripgit_repo_grants (
  repo          TEXT NOT NULL,            -- "owner/name"
  grantee_kind  TEXT NOT NULL CHECK (grantee_kind IN ('team', 'user')),
  grantee_id    TEXT NOT NULL,            -- team.id | "user".id
  role          TEXT NOT NULL CHECK (role IN ('read', 'triage', 'write', 'admin')),
  PRIMARY KEY (repo, grantee_kind, grantee_id)
);

-- Per-org defaults for members with no explicit grant.
CREATE TABLE ripgit_org_settings (
  org_id             TEXT PRIMARY KEY,
  default_repo_role  TEXT NOT NULL DEFAULT 'read'
                     CHECK (default_repo_role IN ('none', 'read', 'write')),
  github_org         TEXT
);
