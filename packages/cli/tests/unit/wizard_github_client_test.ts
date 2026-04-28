/**
 * Unit tests for `src/config/wizard-github-client.ts`.
 *
 * The adapter bridges the wizard's narrow
 * {@link WizardGitHubClient} interface to the App-level
 * {@link AppClient}. These tests exercise:
 *
 *   1. **Happy path** — the adapter reads the private key from the
 *      injected reader, constructs an AppClient with the prompted
 *      credentials, calls `listAppInstallations` once and
 *      `listInstallationRepositories` once per installation, and
 *      projects the join into the `WizardInstallation[]` shape.
 *   2. **Empty installation** — an installation with no accessible
 *      repos surfaces as `WizardInstallation { repositories: [] }`,
 *      not as a hidden installation.
 *   3. **Disk read failure** — a thrown {@link Deno.errors.NotFound}
 *      is rewrapped as a {@link SetupWizardError} so the wizard's
 *      `setup` subcommand prints a tidy single-line diagnostic.
 *   4. **AppClient projection failure** — a
 *      {@link GitHubAppClientError} from the AppClient propagates
 *      through (the wizard's outer catch block in
 *      `fetchInstallations` wraps it as `SetupWizardError`).
 *
 * No real disk or network IO happens: both collaborators are
 * injectable, so the tests hand in scripted in-memory doubles.
 */

import { assertEquals, assertRejects } from "@std/assert";

import { type AppClient, GitHubAppClientError } from "@makina/core";
import { createWizardGitHubClient } from "../../src/config/wizard-github-client.ts";
import { SetupWizardError } from "../../src/config/setup-wizard.ts";

interface ScriptedAppClientCalls {
  readonly factoryArgs: Array<{ appId: number; privateKey: string }>;
  readonly listInstallationsCalls: number;
  readonly listRepositoriesCalls: number[];
}

interface ScriptedAppClientOptions {
  readonly installations?: Array<{
    readonly id: number;
    readonly accountLogin?: string;
  }>;
  readonly repositoriesPerInstallation?: Record<
    number,
    Array<{ readonly owner: string; readonly name: string }>
  >;
  readonly listInstallationsError?: Error;
  readonly listRepositoriesError?: Error;
}

function buildScriptedAppClient(opts: ScriptedAppClientOptions): {
  client: AppClient;
  calls: ScriptedAppClientCalls;
  factoryArgs: Array<{ appId: number; privateKey: string }>;
} {
  const factoryArgs: Array<{ appId: number; privateKey: string }> = [];
  let listInstallationsCalls = 0;
  const listRepositoriesCalls: number[] = [];

  const client: AppClient = {
    listAppInstallations() {
      listInstallationsCalls += 1;
      if (opts.listInstallationsError !== undefined) {
        return Promise.reject(opts.listInstallationsError);
      }
      return Promise.resolve(opts.installations ?? []);
    },
    listInstallationRepositories(installationId) {
      listRepositoriesCalls.push(installationId);
      if (opts.listRepositoriesError !== undefined) {
        return Promise.reject(opts.listRepositoriesError);
      }
      const repos = opts.repositoriesPerInstallation?.[installationId] ?? [];
      return Promise.resolve(repos);
    },
  };

  return {
    client,
    factoryArgs,
    calls: {
      get factoryArgs() {
        return factoryArgs;
      },
      get listInstallationsCalls() {
        return listInstallationsCalls;
      },
      get listRepositoriesCalls() {
        return listRepositoriesCalls;
      },
    },
  };
}

Deno.test("createWizardGitHubClient: happy path projects installations + repos", async () => {
  const { client: appClient, calls, factoryArgs } = buildScriptedAppClient({
    installations: [
      { id: 9876543, accountLogin: "octocat" },
      { id: 12345, accountLogin: "myorg" },
    ],
    repositoriesPerInstallation: {
      9876543: [
        { owner: "octocat", name: "makina" },
        { owner: "octocat", name: "other" },
      ],
      12345: [
        { owner: "myorg", name: "service-a" },
      ],
    },
  });

  let readKeyCalls = 0;
  const wizardClient = createWizardGitHubClient({
    readKeyFile: (path: string) => {
      readKeyCalls += 1;
      assertEquals(path, "~/keys/app.pem");
      return Promise.resolve("PEM-CONTENTS");
    },
    createClient: (opts) => {
      factoryArgs.push({ appId: opts.appId, privateKey: opts.privateKey });
      return appClient;
    },
  });

  const installations = await wizardClient.getInstallations({
    appId: 1234,
    privateKeyPath: "~/keys/app.pem",
  });

  assertEquals(readKeyCalls, 1);
  assertEquals(factoryArgs.length, 1);
  assertEquals(factoryArgs[0]?.appId, 1234);
  assertEquals(factoryArgs[0]?.privateKey, "PEM-CONTENTS");

  assertEquals(calls.listInstallationsCalls, 1);
  assertEquals(calls.listRepositoriesCalls, [9876543, 12345]);

  assertEquals(installations.length, 2);
  assertEquals(installations[0]?.installationId, 9876543);
  assertEquals(installations[0]?.repositories, [
    "octocat/makina",
    "octocat/other",
  ]);
  assertEquals(installations[1]?.installationId, 12345);
  assertEquals(installations[1]?.repositories, ["myorg/service-a"]);
});

Deno.test("createWizardGitHubClient: installation with no repositories surfaces as empty array", async () => {
  const { client: appClient } = buildScriptedAppClient({
    installations: [{ id: 1 }],
    repositoriesPerInstallation: { 1: [] },
  });
  const wizardClient = createWizardGitHubClient({
    readKeyFile: () => Promise.resolve("PEM"),
    createClient: () => appClient,
  });

  const installations = await wizardClient.getInstallations({
    appId: 1,
    privateKeyPath: "/abs/path",
  });

  assertEquals(installations.length, 1);
  assertEquals(installations[0]?.installationId, 1);
  assertEquals(installations[0]?.repositories, []);
});

Deno.test("createWizardGitHubClient: zero installations returns empty array", async () => {
  const { client: appClient } = buildScriptedAppClient({
    installations: [],
  });
  const wizardClient = createWizardGitHubClient({
    readKeyFile: () => Promise.resolve("PEM"),
    createClient: () => appClient,
  });

  const installations = await wizardClient.getInstallations({
    appId: 1,
    privateKeyPath: "/abs/path",
  });
  assertEquals(installations.length, 0);
});

Deno.test("createWizardGitHubClient: disk read failure surfaces as SetupWizardError", async () => {
  const { client: appClient } = buildScriptedAppClient({});
  const wizardClient = createWizardGitHubClient({
    readKeyFile: () => Promise.reject(new Deno.errors.NotFound("no such file")),
    createClient: () => appClient,
  });

  const error = await assertRejects(
    () =>
      wizardClient.getInstallations({
        appId: 1,
        privateKeyPath: "~/missing.pem",
      }),
    SetupWizardError,
  );
  // The diagnostic includes both the path and the underlying reason so a
  // user typing a bad path sees what went wrong without a stack trace.
  assertEquals(error.message.includes("~/missing.pem"), true);
  assertEquals(error.message.includes("no such file"), true);
});

Deno.test("createWizardGitHubClient: AppClient errors propagate verbatim", async () => {
  const { client: appClient } = buildScriptedAppClient({
    listInstallationsError: new GitHubAppClientError(
      "listAppInstallations",
      new Error("403 Bad credentials"),
    ),
  });
  const wizardClient = createWizardGitHubClient({
    readKeyFile: () => Promise.resolve("PEM"),
    createClient: () => appClient,
  });

  // The wizard's outer catch wraps this as SetupWizardError with the
  // message; here we just assert the typed error reaches the boundary.
  const error = await assertRejects(
    () =>
      wizardClient.getInstallations({
        appId: 1,
        privateKeyPath: "/abs/key.pem",
      }),
    GitHubAppClientError,
  );
  assertEquals(error.operation, "listAppInstallations");
});
