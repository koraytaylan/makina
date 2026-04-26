/**
 * makina entry point.
 *
 * Argv-dispatch shell. Today this just exposes the `--version` flag so the
 * release pipeline (and the `build:smoke` CI step) can prove that a compiled
 * binary starts. The real subcommands (`daemon`, `setup`, default TUI) are
 * wired in by the Wave 2+ feature branches; this file is the only entrypoint
 * the eventual `deno compile` target produces, and it imports the version
 * string from `src/constants.ts` so both this stub and any future code share
 * a single source of truth.
 */

import { MAKINA_VERSION } from "./src/constants.ts";

if (Deno.args.includes("--version")) {
  console.log(MAKINA_VERSION);
  Deno.exit(0);
}

console.log("makina — agentic GitHub issue resolver");
console.log("");
console.log("Wave 1 skeleton. Most subcommands are not yet implemented.");
console.log("Run `makina --version` to see the version.");
console.log("See https://github.com/koraytaylan/makina for status.");
