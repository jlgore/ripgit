import type { Env } from "./types";

const GITHUB_AUTHORIZE_URL = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL = "https://github.com/login/oauth/access_token";
const GITHUB_USER_URL = "https://api.github.com/user";

export interface GitHubUser {
  id: number;
  login: string;
  name: string | null;
  email: string | null;
}

/** Build the GitHub OAuth authorization redirect URL. */
export function githubAuthorizeUrl(
  clientId: string,
  state: string,
  callbackUrl: string,
): string {
  const url = new URL(GITHUB_AUTHORIZE_URL);
  url.searchParams.set("client_id", clientId);
  url.searchParams.set("scope", "read:user,user:email");
  url.searchParams.set("state", state);
  url.searchParams.set("redirect_uri", callbackUrl);
  return url.toString();
}

/** Exchange a GitHub authorization code for an access token. */
export async function exchangeCode(
  code: string,
  env: Env,
  callbackUrl: string,
): Promise<string> {
  const resp = await fetch(GITHUB_TOKEN_URL, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      client_id: env.GITHUB_CLIENT_ID,
      client_secret: env.GITHUB_CLIENT_SECRET,
      code,
      redirect_uri: callbackUrl,
    }),
  });

  const data = (await resp.json()) as {
    access_token?: string;
    error?: string;
    error_description?: string;
  };

  if (!data.access_token) {
    throw new Error(
      data.error_description ?? data.error ?? "GitHub token exchange failed",
    );
  }

  return data.access_token;
}

/** Fetch the authenticated GitHub user's profile. */
export async function fetchGitHubUser(githubToken: string): Promise<GitHubUser> {
  const resp = await fetch(GITHUB_USER_URL, {
    headers: {
      Authorization: `Bearer ${githubToken}`,
      "User-Agent": "ripgit-auth/1.0",
    },
  });

  if (!resp.ok) {
    throw new Error(`GitHub API error: ${resp.status} ${resp.statusText}`);
  }

  return resp.json() as Promise<GitHubUser>;
}
