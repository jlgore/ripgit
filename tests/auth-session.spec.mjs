import { beforeAll, afterAll, expect, test } from 'vitest';
import { build } from 'esbuild';
import { Miniflare } from 'miniflare';
import { readFileSync, readdirSync } from 'node:fs';

let mf, db;
let githubLogin = 'octocat';
const origin = 'https://auth.test';
const cookieHeader = (response) => response.headers.getSetCookie().map(c => c.split(';')[0]).join('; ');
async function request(path, init = {}) {
  return mf.dispatchFetch(`${origin}${path}`, { redirect: 'manual', ...init });
}
async function signIn() {
  const login = await request('/login');
  expect(login.status).toBe(302);
  const state = new URL(login.headers.get('location')).searchParams.get('state');
  const callback = await request(`/api/auth/callback/github?code=test-code&state=${encodeURIComponent(state)}`, {
    headers: { Cookie: cookieHeader(login) },
  });
  expect(callback.status).toBe(302);
  expect(callback.headers.get('location')).not.toContain('/error');
  return cookieHeader(callback);
}

beforeAll(async () => {
  const result = await build({
    entryPoints: ['auth/src/index.ts'], bundle: true, write: false,
    format: 'esm', platform: 'neutral', conditions: ['workerd', 'worker', 'browser'],
    external: ['node:*', 'cloudflare:*'], target: 'es2022',
  });
  mf = new Miniflare({
    modules: [{ type: 'ESModule', path: `${process.cwd()}/auth-test-worker.mjs`, contents: result.outputFiles[0].text }],
    compatibilityDate: '2025-01-01', compatibilityFlags: ['nodejs_compat'],
    bindings: {
      BETTER_AUTH_URL: origin, BETTER_AUTH_SECRET: 'test-only-secret-longer-than-thirty-two-characters',
      GITHUB_CLIENT_ID: 'test-client', GITHUB_CLIENT_SECRET: 'test-secret', ORG_CREATORS: 'octocat',
    },
    d1Databases: ['AUTH_DB'], kvNamespaces: ['OAUTH_KV'],
    serviceBindings: { RIPGIT: () => new Response('upstream') },
    outboundService: async (req) => {
      const url = new URL(req.url);
      if (url.hostname === 'github.com' && url.pathname === '/login/oauth/access_token') {
        return Response.json({ access_token: 'test-github-token', token_type: 'bearer', scope: 'read:user,user:email' });
      }
      if (url.hostname === 'api.github.com') {
        if (url.pathname === '/user/emails') return Response.json([{ email: 'octocat@example.test', primary: true, verified: true }]);
        if (url.pathname === '/user' || url.pathname === '/user/12345') {
          return Response.json({ id: 12345, login: githubLogin, name: 'Octocat', email: null, avatar_url: 'https://example.test/avatar' });
        }
      }
      throw new Error(`Unexpected outbound request: ${req.method} ${url.origin}${url.pathname}`);
    },
  });
  db = await mf.getD1Database('AUTH_DB');
  const statements = readdirSync('auth/migrations').sort().flatMap(name => readFileSync(`auth/migrations/${name}`, 'utf8').replace(/^--.*$/gm, '').split(';').map(s => s.trim()).filter(Boolean));
  await db.batch(statements.map(sql => db.prepare(sql)));
});
afterAll(async () => { await mf?.dispose(); });

test('GitHub sign-in persists a protected login, claims its namespace and redirects home', async () => {
  const cookie = await signIn();
  const sessionResponse = await request('/api/auth/get-session', { headers: { Cookie: cookie } });
  const session = await sessionResponse.json();
  expect(session?.user.login).toBe('octocat');
  const namespace = await db.prepare('SELECT name, user_id FROM ripgit_namespaces WHERE name = ?').bind('octocat').first();
  expect(namespace?.user_id).toBe(session.user.id);
  const home = await request('/', { headers: { Cookie: cookie, Accept: 'text/html' } });
  expect(home.status).toBe(302);
  expect(home.headers.get('location')).toBe('/octocat/');

  // A client must never be able to claim somebody else's owner namespace.
  const update = await request('/api/auth/update-user', {
    method: 'POST', headers: { Cookie: cookie, Origin: origin, 'Content-Type': 'application/json' },
    body: JSON.stringify({ login: 'victim' }),
  });
  expect(update.status).toBe(400);
  expect((await db.prepare('SELECT login FROM user WHERE id = ?').bind(session.user.id).first()).login).toBe('octocat');
});

test('an existing session repairs a missing login and namespace without signing in again', async () => {
  const cookie = await signIn();
  await db.batch([
    db.prepare('DELETE FROM ripgit_namespaces'),
    db.prepare('UPDATE user SET login = NULL'),
  ]);
  const home = await request('/', { headers: { Cookie: cookie, Accept: 'text/html' } });
  expect(home.status).toBe(302);
  expect(home.headers.get('location')).toBe('/octocat/');
  const user = await db.prepare('SELECT id, login FROM user').first();
  expect(user.login).toBe('octocat');
  expect((await db.prepare('SELECT user_id FROM ripgit_namespaces WHERE name = ?').bind('octocat').first()).user_id).toBe(user.id);
});

test('repair preserves an owned namespace after a GitHub rename', async () => {
  const cookie = await signIn();
  githubLogin = 'renamed-octocat';
  await db.prepare('UPDATE user SET login = NULL').run();
  const home = await request('/', { headers: { Cookie: cookie, Accept: 'text/html' } });
  expect(home.headers.get('location')).toBe('/octocat/');
  expect((await db.prepare('SELECT login FROM user').first()).login).toBe('octocat');
  githubLogin = 'octocat';
});

test('repair never takes a namespace already owned by another account', async () => {
  const cookie = await signIn();
  await db.batch([
    db.prepare('UPDATE ripgit_namespaces SET user_id = ? WHERE name = ?').bind('other-user', 'octocat'),
    db.prepare('UPDATE user SET login = NULL'),
  ]);
  const home = await request('/', { headers: { Cookie: cookie, Accept: 'text/html' } });
  expect(home.status).toBe(302);
  expect((await db.prepare('SELECT user_id FROM ripgit_namespaces WHERE name = ?').bind('octocat').first()).user_id).toBe('other-user');
});
