/**
 * Evaluate one pipeline file and print its plan as JSON on stdout.
 *
 *   bun /opt/ripgit-ci/sdk/plan.ts <pipeline file> '<event JSON>'
 *
 * Runs inside the plan sandbox, from the repository root. The pipeline file
 * is the repository's code: it runs here, never in the Worker.
 */

import { resolve } from "node:path";
import { planPipeline, type CiEvent, type Plan } from "./plan-core.ts";

const [file, eventJson] = process.argv.slice(2);
let plan: Plan;
try {
  const mod = await import(resolve(process.cwd(), file));
  plan = planPipeline(mod.default, JSON.parse(eventJson) as CiEvent);
} catch (err) {
  plan = { ok: false, error: `could not load ${file}: ${err instanceof Error ? err.message : String(err)}` };
}
// The last line is the plan; anything the pipeline printed comes before it.
process.stdout.write("\n" + JSON.stringify(plan) + "\n");
