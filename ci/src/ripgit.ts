/**
 * ripgit, as seen by the runner: the source archive for a commit, and run
 * status reports. Both go over the service binding as the `ci` actor scoped
 * to one repo; ripgit trusts X-Ripgit-* headers only from its bindings.
 */

export type Report =
  | { type: "plan"; run: number; name: string; matched: boolean; jobs: { name: string; steps: string[] }[] }
  | { type: "job"; run: number; job: string; status: string }
  | { type: "step"; run: number; job: string; index: number; status: string; exitCode?: number; logKey?: string }
  | { type: "run"; run: number; status: string; error?: string };

export class RipgitClient {
  constructor(
    private readonly ripgit: Fetcher,
    readonly owner: string,
    readonly repo: string,
  ) {}

  private headers(extra: Record<string, string> = {}): Headers {
    return new Headers({
      "X-Ripgit-Actor-Name": "ripgit-ci",
      "X-Ripgit-Actor-Kind": "ci",
      "X-Ripgit-Actor-Repo": `${this.owner}/${this.repo}`,
      ...extra,
    });
  }

  private url(path: string): string {
    return `https://ripgit.internal/${encodeURIComponent(this.owner)}/${encodeURIComponent(this.repo)}/${path}`;
  }

  /** The tree at `sha` as a tar stream. */
  async archive(sha: string): Promise<ReadableStream<Uint8Array>> {
    const resp = await this.ripgit.fetch(this.url(`archive/${sha}`), { headers: this.headers() });
    if (!resp.ok || !resp.body) {
      throw new Error(`archive of ${sha} failed: HTTP ${resp.status} ${await resp.text()}`);
    }
    return resp.body;
  }

  async report(report: Report): Promise<void> {
    const resp = await this.ripgit.fetch(this.url("ci/report"), {
      method: "POST",
      headers: this.headers({ "Content-Type": "application/json" }),
      body: JSON.stringify(report),
    });
    if (!resp.ok) {
      throw new Error(`report ${report.type} refused: HTTP ${resp.status} ${await resp.text()}`);
    }
  }
}
