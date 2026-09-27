import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { createTestServer, ownerHeaders, uniqueId } from "./helpers/mf.mjs";

let server;

beforeAll(async () => {
  server = await createTestServer();
});

afterAll(async () => {
  await server.mf.dispose();
});

describe("ripgit core worker", () => {
  test("returns health JSON at root", async () => {
    const response = await server.dispatch("/");

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("application/json");
    await expect(response.json()).resolves.toMatchObject({
      name: "ripgit",
      version: "0.1.0",
    });
  });

  test("serves owner profile markdown when format query overrides Accept", async () => {
    const owner = uniqueId("owner");
    const response = await server.dispatch(`/${owner}/?format=md`, {
      headers: { Accept: "text/html" },
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/markdown; charset=utf-8");
    expect(response.headers.get("vary")).toBeNull();

    const body = await response.text();
    expect(body).toContain(`# ${owner}`);
    expect(body).toContain("Repositories: `0`");
  });

  test("serves owner profile as plain text via Accept and adds Vary", async () => {
    const owner = uniqueId("owner");
    const response = await server.dispatch(`/${owner}/`, {
      headers: { Accept: "text/plain" },
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/plain; charset=utf-8");
    expect(response.headers.get("vary")).toBe("Accept");

    const body = await response.text();
    expect(body).toContain(`# ${owner}`);
    expect(body).toContain("No public repositories.");
  });

  test("rejects unsupported owner profile formats", async () => {
    const owner = uniqueId("owner");
    const response = await server.dispatch(`/${owner}/?format=json`);

    expect(response.status).toBe(406);
    expect(await response.text()).toContain("Requested format 'json' is not available here.");
  });

  test("serves empty repository markdown for the repo owner", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const response = await server.dispatch(`/${owner}/${repo}/?format=md`, {
      headers: ownerHeaders(owner),
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/markdown; charset=utf-8");

    const body = await response.text();
    expect(body).toContain(`# ${owner}/${repo}`);
    expect(body).toContain("Repository is empty.");
    expect(body).toContain("git remote add origin");
    expect(body).toContain("git push origin main");
  });

  test("requires the repo owner for settings", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const response = await server.dispatch(`/${owner}/${repo}/settings`);

    expect(response.status).toBe(401);
    expect(response.headers.get("www-authenticate")).toBe('Basic realm="ripgit"');
    expect(await response.text()).toContain("Unauthorized: sign in to push");
  });

  test("serves owner-only settings in plain text when authorized", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const response = await server.dispatch(`/${owner}/${repo}/settings`, {
      headers: ownerHeaders(owner, { Accept: "text/plain" }),
    });

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/plain; charset=utf-8");
    expect(response.headers.get("vary")).toBe("Accept");

    const body = await response.text();
    expect(body).toContain(`# ${owner}/${repo} settings`);
    expect(body).toContain("Admin-only repository maintenance page.");
    expect(body).toContain("Default branch: `refs/heads/main`");
  });
});
