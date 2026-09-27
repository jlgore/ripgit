import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  appendLineAndCommit,
  cleanupTempDirs,
  cloneFixture,
  git,
  pushAsOwner,
} from "./helpers/git.mjs";
import { actorHeaders, createTestServer, uniqueId } from "./helpers/mf.mjs";

let server;
const tempDirs = [];

function uniqueToken(prefix) {
  return `${prefix}${uniqueId("token")}`.replace(/[^a-zA-Z0-9]/g, "");
}

function formHeaders(actorName) {
  return actorHeaders(actorName, {
    "Content-Type": "application/x-www-form-urlencoded",
  });
}

async function postForm(path, actorName, form = {}) {
  return server.dispatch(path, {
    method: "POST",
    redirect: "manual",
    headers: formHeaders(actorName),
    body: new URLSearchParams(form).toString(),
  });
}

function expectRedirectTo(response, suffix) {
  expect(response.status).toBe(302);
  expect(response.headers.get("location")).toMatch(new RegExp(`${suffix.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`));
}

beforeAll(async () => {
  server = await createTestServer();
});

afterAll(async () => {
  await cleanupTempDirs(tempDirs);
  await server.mf.dispose();
});

describe("issues and pull requests", () => {
  test("creates, comments on, closes, and reopens an issue", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const reporter = uniqueId("reporter");
    const commenter = uniqueId("commenter");
    const issueTitle = `Issue ${uniqueId("title")}`;
    const issueBody = `Issue body ${uniqueId("body")}`;
    const commentBody = `Comment ${uniqueId("comment")}`;

    let response = await postForm(`/${owner}/${repo}/issues`, reporter, {
      title: issueTitle,
      body: issueBody,
    });
    expectRedirectTo(response, `/${owner}/${repo}/issues/1`);

    response = await server.dispatch(`/${owner}/${repo}/issues/1?format=md`);
    expect(response.status).toBe(200);
    let markdown = await response.text();
    expect(markdown).toContain(`# Issue #1: ${issueTitle}`);
    expect(markdown).toContain("- State: `Open`");
    expect(markdown).toContain(issueBody);
    expect(markdown).toContain("No comments yet.");

    response = await postForm(`/${owner}/${repo}/issues/1/comment`, commenter, {
      body: commentBody,
    });
    expectRedirectTo(response, `/${owner}/${repo}/issues/1`);

    response = await postForm(`/${owner}/${repo}/issues/1/close`, reporter);
    expectRedirectTo(response, `/${owner}/${repo}/issues/1`);

    response = await server.dispatch(`/${owner}/${repo}/issues?state=closed&format=md`);
    expect(response.status).toBe(200);
    markdown = await response.text();
    expect(markdown).toContain(issueTitle);

    response = await postForm(`/${owner}/${repo}/issues/1/reopen`, owner);
    expectRedirectTo(response, `/${owner}/${repo}/issues/1`);

    response = await server.dispatch(`/${owner}/${repo}/issues/1?format=md`);
    expect(response.status).toBe(200);
    markdown = await response.text();
    expect(markdown).toContain("- State: `Open`");
    expect(markdown).toContain(commentBody);
    expect(markdown).toContain(commenter);
  });

  test("creates and merges a pull request from a fixture-backed branch", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const contributor = uniqueId("contributor");
    const featureBranch = uniqueId("feature");
    const prTitle = `PR ${uniqueId("title")}`;
    const prBody = `PR body ${uniqueId("body")}`;
    const featureToken = `featuretoken${uniqueId("token")}`.replace(/[^a-zA-Z0-9]/g, "");
    const remoteUrl = new URL(`/${owner}/${repo}`, server.url).toString();

    const source = await cloneFixture();
    tempDirs.push(source.workDir);

    await addRemote(source.repoDir, "ripgit", remoteUrl);
    await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");

    await git(source.repoDir, ["checkout", "-b", featureBranch]);
    await appendLineAndCommit(source.repoDir, "README.md", featureToken, "add feature branch change");
    await pushAsOwner(
      source.repoDir,
      owner,
      "push",
      "ripgit",
      `HEAD:refs/heads/${featureBranch}`,
    );

    let response = await postForm(`/${owner}/${repo}/pulls`, contributor, {
      title: prTitle,
      body: prBody,
      source: featureBranch,
      target: "main",
    });
    expectRedirectTo(response, `/${owner}/${repo}/pulls/1`);

    response = await server.dispatch(`/${owner}/${repo}/pulls/1?format=md`);
    expect(response.status).toBe(200);
    let markdown = await response.text();
    expect(markdown).toContain(`# Pull Request #1: ${prTitle}`);
    expect(markdown).toContain(`- Branches: \`${featureBranch}\` -> \`main\``);
    expect(markdown).toContain(prBody);
    expect(markdown).toContain("README.md");

    response = await server.dispatch(`/${owner}/${repo}/pulls?format=md`);
    expect(response.status).toBe(200);
    markdown = await response.text();
    expect(markdown).toContain(prTitle);

    response = await server.dispatch(`/${owner}/${repo}/search?q=${featureToken}&scope=code`);
    expect(response.status).toBe(200);
    let payload = await response.json();
    expect(payload.total_matches).toBe(0);

    response = await server.dispatch(`/${owner}/${repo}/pulls/1/merge`, {
      method: "POST",
      redirect: "manual",
      headers: actorHeaders(contributor),
    });
    expect(response.status).toBe(403);
    expect(await response.text()).toContain("merging requires write access");

    response = await server.dispatch(`/${owner}/${repo}/pulls/1/merge`, {
      method: "POST",
      redirect: "manual",
      headers: actorHeaders(owner),
    });
    expectRedirectTo(response, `/${owner}/${repo}/pulls/1`);

    response = await server.dispatch(`/${owner}/${repo}/pulls/1?format=md`);
    expect(response.status).toBe(200);
    markdown = await response.text();
    expect(markdown).toContain("- State: `Merged`");
    expect(markdown).toContain("- Merge status: merged in `/");

    response = await server.dispatch(`/${owner}/${repo}/search?q=${featureToken}&scope=code`);
    expect(response.status).toBe(200);
    payload = await response.json();
    expect(payload.total_matches).toBeGreaterThan(0);

    response = await server.dispatch(`/${owner}/${repo}/file?ref=main&path=README.md`);
    expect(response.status).toBe(200);
    expect(await response.text()).toContain(featureToken);
  });

  test("returns 409 for a pull request merge conflict and leaves main unchanged", async () => {
    const owner = uniqueId("owner");
    const repo = uniqueId("repo");
    const contributor = uniqueId("contributor");
    const featureBranch = uniqueId("feature");
    const prTitle = `Conflict PR ${uniqueId("title")}`;
    const featureToken = uniqueToken("featureconflict");
    const mainToken = uniqueToken("mainconflict");
    const remoteUrl = new URL(`/${owner}/${repo}`, server.url).toString();

    const source = await cloneFixture();
    tempDirs.push(source.workDir);
    await addRemote(source.repoDir, "ripgit", remoteUrl);
    await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");

    await git(source.repoDir, ["checkout", "-b", featureBranch]);
    await appendLineAndCommit(
      source.repoDir,
      "README.md",
      `feature conflict token ${featureToken}`,
      "feature-side conflict change",
    );
    await pushAsOwner(
      source.repoDir,
      owner,
      "push",
      "ripgit",
      `HEAD:refs/heads/${featureBranch}`,
    );

    const target = await cloneFixture();
    tempDirs.push(target.workDir);
    await addRemote(target.repoDir, "ripgit", remoteUrl);
    await appendLineAndCommit(
      target.repoDir,
      "README.md",
      `main conflict token ${mainToken}`,
      "main-side conflict change",
    );
    await pushAsOwner(target.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");

    let response = await postForm(`/${owner}/${repo}/pulls`, contributor, {
      title: prTitle,
      body: "conflict body",
      source: featureBranch,
      target: "main",
    });
    expectRedirectTo(response, `/${owner}/${repo}/pulls/1`);

    response = await server.dispatch(`/${owner}/${repo}/pulls/1?format=md`, {
      headers: actorHeaders(owner),
    });
    expect(response.status).toBe(200);
    let markdown = await response.text();
    expect(markdown).toContain("conflicts return `409`");
    expect(markdown).toContain("README.md");

    response = await server.dispatch(`/${owner}/${repo}/pulls/1/merge`, {
      method: "POST",
      redirect: "manual",
      headers: actorHeaders(owner),
    });
    expect(response.status).toBe(409);
    expect(await response.text()).toContain("merge conflict in: README.md");

    response = await server.dispatch(`/${owner}/${repo}/pulls/1?format=md`, {
      headers: actorHeaders(owner),
    });
    expect(response.status).toBe(200);
    markdown = await response.text();
    expect(markdown).toContain("- State: `Open`");
    expect(markdown).toContain("conflicts return `409`");

    response = await server.dispatch(`/${owner}/${repo}/file?ref=main&path=README.md`);
    expect(response.status).toBe(200);
    const mainReadme = await response.text();
    expect(mainReadme).toContain(mainToken);
    expect(mainReadme).not.toContain(featureToken);

    response = await server.dispatch(`/${owner}/${repo}/search?q=${featureToken}&scope=code`);
    expect(response.status).toBe(200);
    let payload = await response.json();
    expect(payload.total_matches).toBe(0);

    response = await server.dispatch(`/${owner}/${repo}/search?q=${mainToken}&scope=code`);
    expect(response.status).toBe(200);
    payload = await response.json();
    expect(payload.total_matches).toBeGreaterThan(0);
  });
});
