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

const subcommand = Deno.args[0];
if (subcommand === "daemon" || subcommand === "setup") {
  // Owned by the `daemon` (#9) and `setup` (#3) Wave 2 branches; until
  // they land here the command exits with a clear "not yet wired"
  // notice rather than dropping into the TUI launch path.
  console.log(`subcommand "${subcommand}" not yet wired in this branch.`);
  Deno.exit(0);
} else {
  // Default branch → launch the Ink-based TUI.
  await import("./src/tui/App.tsx").then((module) => module.launch());
}
