# Artifacts in ripgit

Open **Artifacts** from your profile or account settings, or visit
`/settings/artifacts`. The page lists the deployment's bound Artifacts namespace,
including import status and pagination, and your linked ripgit repositories.

1. Choose an existing Artifacts repository and a new local repository name, then
   select **Link privately**. Alternatively, import a public HTTPS Git URL into
   a new Artifacts repository and link it in one step.
2. Wait until an import shows `ready`, then select **Sync now**.
3. Open the linked repository to browse code, commits and diffs in ripgit.
   Sync again to pull subsequent upstream changes. This page does not schedule
   automatic synchronization or push changes back to Artifacts.

New links are private. Change visibility deliberately in the repository's
Settings after syncing. Linking refuses a populated repository or an existing
upstream. A mirror promoted to accept local writes must be reconciled before
syncing, to avoid overwriting those writes. Repository names remain permanent.
The linked-repo index uses KV, so the list can lag briefly; the confirmation
page reads the repository's current status directly.

## Deployment

Keep deployment values in your deployment repository. On the **core** Worker,
configure the `ARTIFACTS` namespace binding and an `ARTIFACTS_ADMINS` variable
containing comma-separated, permanent ripgit usernames of deployment operators:

```json
{
  "vars": { "ARTIFACTS_ADMINS": "your-username" },
  "artifacts": [{ "binding": "ARTIFACTS", "namespace": "your-namespace" }]
}
```

Access is denied when this allowlist is absent. Listing and link/import/sync
require a browser-session user in the allowlist; API keys and mirror tokens do
not grant deployment-wide Artifacts access. Mutations additionally require
admin permission on the destination repository. The browser page creates
links under the signed-in user's own namespace. Artifacts tokens never appear
in the page or listing response.

Deploy both core and auth for the UI and API changes. No database migration
is needed. Existing links are added to the new linked-repo index on their next
successful sync.

Backend routes:

- `GET /api/artifacts?cursor=...&links_cursor=...`: paginated namespace and local links.
- `GET /:owner/:repo/artifacts`: link status, under normal repository visibility.
- `POST /:owner/:repo/artifacts/link`: link, create, import or external remote JSON.
- `POST /:owner/:repo/artifacts/sync`: sync a linked upstream.

The implementation uses the [Artifacts Workers binding](https://developers.cloudflare.com/artifacts/api/workers-binding/).

## Validation

Install root and auth dependencies (`npm ci` and `npm --prefix auth ci`), build
the Rust worker, then run:

```sh
NODE_OPTIONS=--experimental-vm-modules npx vitest run tests/artifacts.spec.mjs tests/auth-session.spec.mjs
npm --prefix auth run typecheck
```

The Artifacts runtime tests use an RPC service stub and exercise the UI against
the real Rust worker, D1 and Durable Objects. Remote namespace availability and
nonempty production sync still depend on the configured Cloudflare resources.
