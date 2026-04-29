/**
 * Integration test for the setup wizard's real GitHub App auth path.
 *
 * Gated by `MAKINA_SETUP_REAL_TEST=1`. When unset (the default for CI),
 * the test skips: it requires real App credentials and a sandbox
 * installation that the developer running the test owns. Skipping
 * loudly is preferable to a no-op `Deno.test.ignore` so the developer
 * who does have credentials sees the gating env var name.
 *
 * The `MAKINA_SETUP_REAL_*` env vars (only read when the gate is set):
 *
 *   - `MAKINA_SETUP_REAL_APP_ID` — numeric App id.
 *   - `MAKINA_SETUP_REAL_PRIVATE_KEY_PATH` — absolute path to the App's
 *     PEM private key. The test does **not** expand `~/` here; pass the
 *     literal path the runtime user can read.
 *   - `MAKINA_SETUP_REAL_EXPECTED_REPO` — `<owner>/<repo>` slug the
 *     developer's sandbox install grants access to. The test asserts
 *     it shows up in the discovered list. Optional; if unset the test
 *     only asserts that *some* installation surfaced.
 *
 * The test invokes the production
 * {@link createWizardGitHubClient} (no scripted doubles), exercising
 * the full wire path: `@octokit/auth-app` mints a JWT, we call
 * `GET /app/installations`, and for each installation we call
 * `GET /installation/repositories`. A successful run proves the wiring
 * is end-to-end functional against real GitHub. The test does not
 * write a `config.json` — that branch is owned by the integration
 * test in `tests/integration/setup_wizard_test.ts` which uses the
 * scripted client.
 */

import { assert, assertEquals } from "@std/assert";

import { createWizardGitHubClient } from "../../src/config/wizard-github-client.ts";

const GATE_VAR = "MAKINA_SETUP_REAL_TEST";
const APP_ID_VAR = "MAKINA_SETUP_REAL_APP_ID";
const KEY_PATH_VAR = "MAKINA_SETUP_REAL_PRIVATE_KEY_PATH";
const EXPECTED_REPO_VAR = "MAKINA_SETUP_REAL_EXPECTED_REPO";

function gateActive(): boolean {
  return Deno.env.get(GATE_VAR) === "1";
}

Deno.test({
  name: "setup wizard: real GitHub App auth lists installations + repos",
  ignore: !gateActive(),
  async fn() {
    const rawAppId = Deno.env.get(APP_ID_VAR);
    const keyPath = Deno.env.get(KEY_PATH_VAR);
    const expectedRepo = Deno.env.get(EXPECTED_REPO_VAR);
    assert(
      rawAppId !== undefined && rawAppId.length > 0,
      `${GATE_VAR}=1 requires ${APP_ID_VAR} (the App's numeric id).`,
    );
    assert(
      keyPath !== undefined && keyPath.length > 0,
      `${GATE_VAR}=1 requires ${KEY_PATH_VAR} (absolute PEM path).`,
    );
    const appId = Number.parseInt(rawAppId, 10);
    assert(
      Number.isInteger(appId) && appId > 0,
      `${APP_ID_VAR} must be a positive integer; got ${JSON.stringify(rawAppId)}`,
    );

    const client = createWizardGitHubClient();
    const installations = await client.getInstallations({
      appId,
      privateKeyPath: keyPath,
    });

    assert(
      installations.length > 0,
      "expected at least one installation for the configured App; got zero",
    );
    if (expectedRepo !== undefined && expectedRepo.length > 0) {
      const allRepos = installations.flatMap((installation) => installation.repositories);
      assertEquals(
        allRepos.includes(expectedRepo),
        true,
        `expected ${expectedRepo} to appear among ${JSON.stringify(allRepos)}`,
      );
    }
  },
});
