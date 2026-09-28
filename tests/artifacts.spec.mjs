import { afterAll, beforeAll, expect, test } from 'vitest';
import { createTestServer, actorHeaders } from './helpers/mf.mjs';
import { handleArtifacts } from '../auth/src/artifacts.ts';
let server;
beforeAll(async () => { server = await createTestServer({ artifacts: true }); });
afterAll(async () => { await server?.mf.dispose(); });
const headers = actorHeaders('alice', { 'Content-Type': 'application/json' });
const post = (path, body, extra = {}) => server.dispatch(path, { method: 'POST', headers: { ...headers, ...extra }, body: JSON.stringify(body) });

test('namespace listing is operator-only, paginated and never includes tokens', async () => {
  for (const actor of [undefined, actorHeaders('bob'), actorHeaders('alice', { 'X-Ripgit-Actor-Kind': 'agent' })]) {
    expect((await server.dispatch('/api/artifacts', { headers: actor })).status).toBe(403);
  }
  const first = await server.dispatch('/api/artifacts', { headers });
  expect(first.status).toBe(200);
  expect(first.headers.get('cache-control')).toContain('no-store');
  expect((await server.dispatch('/api/artifacts', { headers: actorHeaders('alice', { 'X-Ripgit-Actor-Id': 'recycled-github-login' }) })).status).toBe(403);
  const page = await first.json();
  expect(page.repos[0].name).toBe('starter');
  expect(page.cursor).toBe('page-2');
  expect(JSON.stringify(page)).not.toContain('token');
  const next = await server.dispatch('/api/artifacts?cursor=page-2', { headers });
  expect((await next.json()).repos[0].name).toBe('second');
});

test('link is private, listed on the profile, cannot be replaced, and can sync', async () => {
  const link = await post('/alice/project/artifacts/link', { repo: 'starter' });
  expect(link.status).toBe(200);
  expect((await server.dispatch('/alice/project/')).status).toBe(404);
  const profile = await server.dispatch('/alice/', { headers });
  expect(await profile.text()).toContain('/alice/project');
  const replace = await post('/alice/project/artifacts/link', { repo: 'second' });
  expect(replace.status).toBe(409);
  const sync = await post('/alice/project/artifacts/sync', {});
  expect(sync.status, await sync.text()).toBe(200);
  const status = await server.dispatch('/alice/project/artifacts', { headers });
  expect((await status.json()).last_sync).toBeTruthy();
  const list = await server.dispatch('/api/artifacts', { headers });
  expect((await list.json()).links).toEqual(expect.arrayContaining([expect.objectContaining({ local: 'project', repo: 'starter' })]));
});

test('operators still need repo admin and cross-origin mutations fail', async () => {
  await server.dispatch('/bob/', { headers: actorHeaders('bob') });
  expect((await post('/bob/other/artifacts/link', { repo: 'starter' })).status).toBe(403);
  expect((await post('/alice/csrf/artifacts/link', { repo: 'starter' }, { Origin: 'https://evil.test' })).status).toBe(403);
  expect((await post('/bob/own/artifacts/link', { repo: 'starter' }, actorHeaders('bob'))).status).toBe(403);
});

const actor = { userId: 'alice-id', login: 'alice', kind: 'user', scopes: [] };
const uiOrigin = 'https://auth.test';
const pageRequest = (body, origin = uiOrigin) => new Request(`${uiOrigin}/settings/artifacts`, body ? { method: 'POST', headers: { Origin: origin }, body: new URLSearchParams(body) } : {});
const forward = async request => server.dispatch(new URL(request.url).pathname + new URL(request.url).search, {
  method: request.method, headers: { ...headers, ...(request.method === 'POST' ? { Origin: server.url.origin } : {}) },
  body: request.method === 'POST' ? await request.text() : undefined,
});

test('browser page exposes listing, import, sync and escapes repository names', async () => {
  const response = await handleArtifacts(pageRequest(), actor, forward);
  const html = await response.text();
  expect(html).toContain('Namespace repositories (2)');
  expect(html).toContain('Link privately');
  expect(html).toContain('Import and link privately');
  expect(html).toContain('Sync now');
  expect(html).not.toContain('secret-not-for-browser');
  const escaped = await handleArtifacts(pageRequest(), actor, async () => Response.json({ repos: [{ name: '<script>alert(1)</script>', defaultBranch: 'main' }], links: [], total: 1 }));
  expect(await escaped.text()).not.toContain('<script>alert(1)</script>');
});

test('browser link and import forward validated actions and redirect to persistent status', async () => {
  const link = await handleArtifacts(pageRequest({ action: 'link', local: 'via-ui', repo: 'starter' }), actor, forward);
  expect(link.status).toBe(302);
  expect(link.headers.get('location')).toContain('local=via-ui');
  const imported = await handleArtifacts(pageRequest({ action: 'import', local: 'imported', name: 'new-artifact', source: 'https://github.com/example/repo.git' }), actor, forward);
  expect(imported.status).toBe(302);
  const status = await server.dispatch('/alice/imported/artifacts', { headers });
  expect((await status.json()).repo).toBe('new-artifact');
});

test('browser refuses unsafe paths, credentials in import URLs and cross-origin forms', async () => {
  for (const body of [
    { action: 'link', local: '../bob', repo: 'starter' },
    { action: 'import', local: 'safe', name: 'safe', source: 'https://token@github.com/example/repo' },
    { action: 'import', local: 'safe', name: 'safe', source: 'http://github.com/example/repo' },
  ]) expect((await handleArtifacts(pageRequest(body), actor, forward)).status).toBe(400);
  expect((await handleArtifacts(pageRequest({ action: 'sync', local: 'project' }, 'https://evil.test'), actor, forward)).status).toBe(403);
  expect((await handleArtifacts(pageRequest(), null, forward)).headers.get('location')).toContain('/login');
});
