/**
 * pre_commit.ts — fast local gate run by the .githooks/pre-commit hook.
 *
 * Runs the subset of `deno task ci` that completes in a second or two so it
 * does not become a friction tax on every commit. The full coverage and
 * compile-smoke gates run in CI.
 *
 * Steps (in order; first failure stops the run):
 *   1. deno fmt --check
 *   2. deno lint
 *   3. deno check packages/cli/main.ts
 */

interface Step {
  readonly name: string;
  readonly args: readonly string[];
}

const steps: readonly Step[] = [
  { name: "deno fmt --check", args: ["fmt", "--check"] },
  { name: "deno lint", args: ["lint"] },
  { name: "deno check packages/cli/main.ts", args: ["check", "packages/cli/main.ts"] },
];

const EXIT_FAILURE = 1;

let failed = false;
for (const step of steps) {
  console.log(`> ${step.name}`);
  const command = new Deno.Command("deno", {
    args: [...step.args],
    stdout: "inherit",
    stderr: "inherit",
  });
  const result = await command.output();
  if (!result.success) {
    failed = true;
    break;
  }
}

if (failed) {
  console.error("");
  console.error("pre-commit gate failed. Fix the issues above, or re-run the");
  console.error("commit with `--no-verify` and a written justification.");
  Deno.exit(EXIT_FAILURE);
}
