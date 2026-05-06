/**
 * install.ts — compile `packages/cli/main.ts` directly into a directory
 * on the user's `$PATH`.
 *
 * Wired in by `deno task install`. The destination defaults to
 * `${HOME}/.local/bin/makina` and can be overridden with the
 * `MAKINA_INSTALL_DIR` env var (e.g. `/usr/local/bin`).
 *
 * Why a script and not an inline `deno task` shell pipeline? `deno`'s
 * task shell does not support `${VAR:-default}` parameter expansion,
 * so the env-var override has to live in real TypeScript.
 */

const installDir = Deno.env.get("MAKINA_INSTALL_DIR") ??
  `${requireHome()}/.local/bin`;
const target = `${installDir}/makina`;

await Deno.mkdir(installDir, { recursive: true });

const compile = new Deno.Command(Deno.execPath(), {
  args: ["compile", "-A", `--output=${target}`, "packages/cli/main.ts"],
  stdout: "inherit",
  stderr: "inherit",
});
const { code } = await compile.output();
if (code !== 0) {
  Deno.exit(code);
}

console.log(`Installed makina → ${target}`);
console.log(
  `Make sure ${installDir} is on your $PATH (e.g. add it to ~/.zshrc).`,
);

/** Read `$HOME` or fail fast — every supported platform sets it. */
function requireHome(): string {
  const home = Deno.env.get("HOME");
  if (home === undefined || home.length === 0) {
    console.error(
      "install: $HOME is not set; either set $HOME or pass MAKINA_INSTALL_DIR.",
    );
    Deno.exit(1);
  }
  return home;
}
