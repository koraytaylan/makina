/**
 * makina entry point.
 *
 * This is the Wave 0 stub. It exposes the `--version` flag so the release
 * pipeline (and the `build:smoke` CI step) can prove that a compiled binary
 * starts. All real subcommands (`daemon`, `setup`, default TUI) are wired in
 * by the wave 1+ feature branches.
 *
 * The version constant is duplicated here intentionally for the bootstrap.
 * Wave 1 introduces `src/constants.ts` as the single source of truth and this
 * file imports from there.
 */

const MAKINA_VERSION = "0.0.0-dev";

if (Deno.args.includes("--version")) {
  console.log(MAKINA_VERSION);
  Deno.exit(0);
}

console.log("makina — agentic GitHub issue resolver");
console.log("");
console.log("Wave 0 skeleton. Most subcommands are not yet implemented.");
console.log("Run `makina --version` to see the version.");
console.log("See https://github.com/koraytaylan/makina for status.");
