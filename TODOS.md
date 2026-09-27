# ripgit — current work and future tasks

## Current state

The original core build plan is implemented and compiling with zero warnings.
The project has also moved past that baseline: push, clone, fetch, the read
API, diff engine, FTS5 search, issues/PRs, agent-readable markdown/plain page
views, the optional GitHub OAuth auth worker, and the new root test harness
all work.

Tested with:
- **235-commit repo** — 5.3x compression ratio via xpatch
- **cloudflare/agents** — 13,464 objects, 11.4 MiB pack, pushes in one shot
- **git/git** — 80K commits, pushed incrementally to fp 14,000 (checkpoint pushes)

### Code structure

```
src/
  lib.rs          — Worker entry point, routing, owner profile page, content
                    negotiation dispatch, ref advertisement, admin endpoints
  presentation.rs — `?format` + `Accept` negotiation, markdown/plain helpers,
                    action rendering, shared text-mode hints, `Vary: Accept`
  schema.rs       — 11 tables + 3 FTS5 virtual tables + indexes
                    (refs, commits, commit_parents, commit_graph, trees,
                    blob_groups, blobs, blob_chunks, raw_objects, config,
                    fts_head, fts_commits)
  pack.rs         — Streaming pack parser: build_index (decompress-to-sink),
                    resolve_type (OFS_DELTA chain following), resolve_entry
                    (on-demand decompression, Arc-based ResolveCache with byte
                    budget, ResolveCtx bundle), pack generator for fetch.
                    MAX_PACK_BYTES (50 MB) and CACHE_BUDGET_BYTES (20 MB).
  store.rs        — Storage layer: commit/tree/blob parsing, xpatch delta
                    compression with zlib-compressed keyframes + blob_chunks,
                    batched SQL INSERTs, binary lifting commit graph,
                    blob reconstruction, config helpers, incremental FTS
                    rebuild, search (FTS5 + INSTR), lossy UTF-8
  git.rs          — Git smart HTTP protocol: receive-pack (pack body size
                    gate, streaming pack processing, two-phase push handling,
                    dynamic default branch, FTS trigger), upload-pack (fetch)
  api.rs          — Read API: refs, log, commit, tree, blob, file-at-ref,
                    search (code + commits, @prefix: column filter syntax),
                    stats (using stored_size column, no full table scan)
  diff.rs         — Diff engine: recursive tree comparison, line-level diffs
                    (via `similar`), commit diff, two-commit compare
  issues.rs       — Issues/PR storage, comments, merge-base search, three-way
                    merge, merge commit creation, form parsing utilities
  web.rs          — Shared HTML shell/CSS, markdown rendering, owner profile,
                    repo README helpers, diff rendering, raw/blob helpers
  web/            — Repo home, log, tree/blob, search, settings, commit/diff
                    HTML + markdown renderers
  issues_web.rs   — Shared issues/PR web helpers and re-exports
  issues_web/     — Issues/pulls list, detail, and form HTML + markdown
                    renderers

auth/
  src/index.ts    — GitHub OAuth front worker, browser sessions, agent tokens,
                    trusted header forwarding, text-mode landing/settings
  README.md       — Setup, deploy, bindings/secrets, and text-mode docs

tests/
  helpers/mf.mjs  — Miniflare test server factory for the core worker
  helpers/git.mjs — temp repo + git CLI helpers for fixture-based e2e tests
  worker-smoke.spec.mjs
                  — negotiated representation and auth smoke tests
  git-e2e.spec.mjs
                  — real-world push/clone/fetch/force-push coverage
  fixtures/       — pinned offline git fixture bundles and refresh notes
```

---

## Completed

All items below have been implemented and verified.

### Search improvements

1. **All matches with line numbers** — After FTS5 identifies matching files,
   scans content line-by-line and returns every match with its line number.
   Web UI links to `/blob/:ref/:path#L47`.

2. **Literal / exact substring search** — Auto-detects symbol-heavy queries
   (`.`, `_`, `()`, `::`) and falls back to `INSTR(content, ?)`. Full table
   scan but bounded by repo size. `lit:` prefix also forces literal mode.

3. **Scope filters / @prefix: syntax** — `@path:src/`, `@ext:rs`, `@author:`,
   `@message:`, `@content:` inline query prefixes replace separate form fields.
   Parsed in `api::parse_search_query`, strips `@` and maps to FTS5 column
   filters or SQL LIKE predicates. Auto-routes scope (code vs commits) from
   the prefix used. Works for both FTS5 and INSTR modes.

4. **Commit message search** — `fts_commits` FTS5 table indexed on hash,
   message, author. Populated during push. Exposed via `?scope=commits` and
   a tab in the web UI search page.

5. **Incremental FTS rebuild** — Uses `diff::diff_trees` to compare old and
   new HEAD trees. Only inserts/deletes/updates changed files in `fts_head`.
   Stores last indexed commit hash in `config` table. O(changed files) per push.

6. **Default branch detection** — First branch pushed becomes the default.
   Stored in `config` table. FTS rebuild triggers on any push to the default
   branch (not hardcoded to `main`).

### Web + agent UI

7. **Branch selector** — Dropdown on home, tree, blob, and log pages listing
   all `refs/heads/*` entries. Shows current branch prominently. Selecting a
   branch navigates to the same page on that branch.

8. **Agent-readable representations** — Page routes negotiate `text/html`,
   `text/markdown`, and `text/plain`. `?format=` overrides `Accept`. Responses
   chosen from `Accept` add `Vary: Accept`.

9. **Markdown/plain page coverage** — Owner profile, repo home, commits, tree,
   blob, commit, diff, search, settings, issues, pulls, issue/PR detail, and
   new issue/new PR forms all have explicit text renderers.

10. **Shared presentation layer** — `src/presentation.rs` centralizes
    negotiation, markdown/plain response helpers, action descriptions, section
    rendering, and the shared navigation hint for agents.

11. **Web module split** — Repo pages live in `src/web/*`; issue/PR pages live
    in `src/issues_web/*`. `src/web.rs` and `src/issues_web.rs` stay as shared
    shells/helpers.

12. **Issues and pull requests** — SQLite-backed issues/PRs with list/detail
    pages, new issue/new PR forms, comments, open/close/reopen actions, and
    repo-owner merge.

13. **PR merge flow** — Merge-base search plus fast-forward or three-way tree
    merge inside the DO. Stores the merge commit and updates the target ref.

14. **Markdown rendering** — Replaced the hand-rolled renderer with
    `pulldown-cmark`. Supports tables, footnotes, strikethrough, task lists,
    and smart punctuation. Raw HTML is escaped and unsafe URLs are neutralized.

15. **Repo-aware README links** — Relative README links and images on the repo
    home page are rewritten against the current ref so in-repo navigation works
    (`/blob`, `/tree`, `/raw`).

16. **Syntax highlighting** — highlight.js CDN with line numbers plugin.
    `#L` anchor support for deep linking to specific lines.

17. **Persistent nav search with live results** — Search bar in the nav on
    every page. Fetches `/search?q=...` on each keystroke (200ms debounce),
    shows a dropdown of file paths + first matching line (code) or commit hash
    + message (commits). Enter navigates to the full search page. Scope
    (code vs commits) detected client-side from `@author:`/`@message:` prefixes.

18. **Repo bar layout fix** — Global nav stays full width while the repo
    secondary bar uses a full-width wrapper with centered inner contents.

### Protocol fixes

19. **HEAD symbolic ref** — `advertise_refs` includes `HEAD` pointing to the
    default branch via `symref=HEAD:refs/heads/:name` capability. Fixes
    `git clone` for repos whose default branch isn't `main`.

20. **Clone with non-main branch** — Consequence of #19. `git clone` now checks
    out the correct branch automatically.

21. **Two-phase push handling** — When `git push` sends a payload larger than
    `http.postBuffer` (1 MiB default), git sends a 4-byte probe (`0000`) then
    the full payload with chunked encoding. Fixed by returning 200 OK for
    empty command sets.

### Performance + scale

22. **Streaming pack parser** — Replaced the all-in-memory `parse()` with a
    two-pass approach: index pass (decompress-to-sink, ~100 bytes/entry) then
    process-by-type (decompress on-demand from pack bytes). Peak memory went
    from >128 MiB (OOM) to ~15 MiB for a 13K-object pack.

23. **Resolve cache** — Bounded 1024-entry cache for resolved pack entries.
    Caches delta chain bases and intermediates to avoid re-decompressing shared
    bases. Critical for git packs with depth-50 chains — reduces decompressions
    by 5-10x for packs with many objects sharing base chains.

24. **Keyframe compression** — Keyframes (full blob snapshots, every 50
    versions) are zlib-compressed before storage. A 5 MB source file compresses
    to ~500 KB. Deltas are left as-is (xpatch uses zstd internally). Zero cost
    for the common case (all files fit in single rows after compression).

25. **Blob chunking** — `blob_chunks` overflow table for compressed keyframes
    that still exceed DO's 2 MB row limit. Transparent to all read paths —
    `reconstruct_blob` reassembles chunks automatically. Only activates for
    large binary files.

26. **Batched SQL INSERTs** — Tree entries batched 25 per statement (4 params
    each, under DO's 100 bound parameter limit). Commit parents batched 33 per
    statement. Cuts total SQL operations by ~6x for large pushes.

27. **Fast existence checks** — Replaced `SELECT COUNT(*) AS n` with
    `SELECT 1 LIMIT 1` for dedup checks in store_commit, store_tree,
    store_blob. Indexed PK lookup, instant return.

28. **stored_size column** — Tracks compressed blob size at INSERT time.
    Stats endpoint uses `SUM(stored_size)` over an integer column instead of
    `SUM(LENGTH(data))` which would scan every data page. Instant stats
    regardless of repo size.

29. **Lossy UTF-8** — `String::from_utf8_lossy` for commit parsing. Old repos
    with Latin-1 or other non-UTF-8 author names are handled gracefully.
    Raw bytes preserved in `raw_objects` for byte-identical fetch.

30. **Admin endpoints** — `DELETE /repo/:name/` wipes all tables.
    `PUT /repo/:name/admin/set-ref` manually sets a ref for recovery from
    partial push timeouts.

31. **Arc-based zero-copy resolve cache** — `ResolveCache` stores `Arc<[u8]>`
    instead of `Vec<u8>`. Cache hits return `Arc::clone` (pointer increment,
    no data copy). `ExternalObjects` also uses `Arc<[u8]>`. `resolve_entry`
    returns `Arc<[u8]>` — each decompressed object is allocated exactly once
    and shared between the cache and the caller. During a processing loop,
    the Arc is at refcount 2 (cache + caller); caller drops at end of iteration,
    leaving refcount 1 in cache. `cache.clear()` drops the last reference.

32. **Budget enforcement** — `MAX_PACK_BYTES = 50 MB` hard gate in
    `handle_receive_pack`: packs above this return a proper `ng` pkt-line
    response before any object is parsed. `CACHE_BUDGET_BYTES = 20 MB`
    enforced inside `ResolveCache::try_cache` — cache silently stops growing
    when the byte budget is exhausted; processing continues via re-decompression.
    Peak memory ceiling at a 50 MB push: ~85 MB (40 MB below the 128 MB wall).
    `ResolveCtx` bundles cache + external objects for `resolve_entry`.

### Auth + docs

33. **Auth worker text mode** — `auth` landing page and
    `/settings` also negotiate markdown/plain views for curl/agents.

34. **Docs refresh** — `README.md` documents text-mode navigation and curl
    examples. `auth/README.md` covers setup, deploy,
    bindings/secrets, and text-mode behavior.

### Testing + fetch negotiation

35. **Root test harness** — Added a root `package.json` + `vitest`/`miniflare`
    setup for the core ripgit worker only. Tests boot the built worker with the
    real KV + SQLite DO bindings under Miniflare.

36. **Rust protocol/unit coverage** — Added unit tests for representation
    negotiation, search query parsing, URL query decoding, and upload-pack
    negotiation helpers.

37. **Real-world git fixture e2e** — Added a pinned offline
    `tests/fixtures/workers-rs-main.bundle` fixture and a git CLI e2e suite
    covering push, clone, fast-forward push, force-push, search refresh, and
    fetch from an existing clone.

38. **Fetch after force-push** — Fixed `git fetch` for existing clones after a
    non-fast-forward rewrite by advertising upload-pack capabilities separately
    from receive-pack and by implementing the expected ACK/NAK negotiation
    before streaming the pack.

---

## Known limitations

These are documented, accepted trade-offs — not bugs.

- **Auth is upstream** — ripgit expects trusted `X-Ripgit-Actor-*` headers
  from an auth worker or other front proxy. Reads are public; writes return
  401 without a trusted actor.
- **DO storage timeout** — Pushes with many objects (>~10K per incremental
  push) can exceed the DO's ~30 second storage operation timeout. Each
  `sql.exec()` auto-commits individually (no request-level transaction).
  Cloudflare's `transactionSync()` API would provide atomicity but is not
  exposed in workers-rs 0.7.5. Use the admin/set-ref endpoint to recover
  from partial push state.
- **50 MB pack body limit (server-enforced)** — `MAX_PACK_BYTES` in `pack.rs`
  rejects packs above 50 MB with a clean `ng` response before any object is
  parsed. The hard Workers platform limit is 100 MB, but we gate lower to keep
  peak DO memory well under the 128 MB ceiling. Repos must be pushed
  incrementally via the push script's checkpoint mechanism.
- **Force pushes are always allowed** — non-fast-forward updates currently work,
  but there is no repo setting or policy hook to reject them when a repo wants
  branch protection semantics.
- **No annotated tag objects** — Silently dropped during push. Lightweight
  tags (refs) work fine.
- **Timezone lost in parsed commits table** — The `commits` table stores unix
  timestamps without timezone offset. The `raw_objects` table preserves the
  original bytes for fetch, but the `/log` API shows times without timezone.
- **README fragment-only heading links** — Relative README file/dir/image links
  are rewritten on the repo home page, but bare `#heading` fragments are not
  yet translated to GitHub-style generated heading IDs.
- **side-band-64k** — Removed from advertised capabilities to avoid wrapping
  the report-status response with sideband bytes.


---

## Next up

### 1. File history

Show all commits that touched a specific file. Walk the first-parent commit
chain, resolve the file path in each commit's tree, emit a result when the
blob hash changes. O(commits * path_depth) — fast for DO-sized repos.

- **API**: `GET /history?ref=main&path=src/lib.rs` — returns list of commits
  that modified the file, with timestamps, authors, and messages.
- **Web UI**: linked from the blob viewer. Paginated commit list scoped to
  one file.

### 2. Blame

Attribute each line of a file to the commit that last modified it. Leverages
blob_groups (all versions of a file by path) and the diff engine (line-level
diffs).

- **API**: `GET /blame?ref=main&path=src/lib.rs` — returns lines with commit
  hash, author, timestamp per line.
- **Web UI**: blame view linked from the blob viewer. Line numbers, commit
  info column, file content.

### 3. Tags page

Browse lightweight tags in the web UI. Already stored in the `refs` table as
`refs/tags/*`. Quick win — just a new page listing tags with their target
commit info.

- **Web UI**: `/repo/:name/tags` — list of tags with commit hash, author,
  date, and message. Link to commit detail page.

---

## Potential future work

- **transactionSync binding** — Add a custom wasm_bindgen binding for
  `ctx.storage.transactionSync()` to get atomic push semantics. Prevents
  partial state on DO timeout. The JS API exists, workers-rs just doesn't
  expose it yet.
- **Repository index** — KV side-index written on push, landing page listing
  all repos with stats. Needs a KV binding in `wrangler.toml`.
- **Annotated tags** — Parse and store tag objects (separate from lightweight
  tag refs). Requires a `tag_objects` table + pack parser changes.
- **Alternative auth frontends** — Bearer token-only or Cloudflare Access
  integration beyond the GitHub OAuth example.
- **Force-push policy** — Add optional rejection or branch-protection rules for
  non-fast-forward updates instead of always allowing them.
- **Streaming zlib compression** — Currently `blob_zlib_compress` buffers the
  entire compressed output (2x blob size in memory). Switching to
  `flate2::write::ZlibEncoder` with incremental chunk writes would eliminate
  the compressed copy, reducing peak memory from `raw + compressed` to
  `raw + ~256 KB`. Compression ratio is identical (single continuous zlib
  stream). Main blocker: interleaves compression with `blob_chunks` INSERT
  logic, changing the `store_blob` flow. Worth doing for medium-sized blobs
  (10-50 MB) where the compressed copy is significant.
- **side-band-64k** — Re-add with proper sideband wrapping for progress
  reporting.
- **Selective page JSON** — Consider page-model JSON for page-only routes such
  as owner profile, repo home, settings, and auth worker pages if agents need
  it; keep the resource JSON API canonical.
- dont use fetch from DO. expose rpc methods, let worker call the right one.
