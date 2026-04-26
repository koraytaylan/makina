/**
 * Integration tests for `src/config/setup-wizard.ts`.
 *
 * The wizard's collaborators are injected (`readLine`, `writeLine`,
 * `githubClient`) so these tests script a full first-run conversation
 * end-to-end without touching real stdin or the network. The
 * happy-path test pipes scripted answers into a stub
 * {@link WizardGitHubClient} and asserts that the wizard produces a
 * config that re-validates against the W1 zod schema. The negative
 * tests cover the validation paths the wizard owns: missing private
 * key, no installations reachable, malformed numeric input, and EOF on
 * stdin.
 */

import { assert, assertEquals, assertRejects } from "@std/assert";

import { parseConfig } from "../../src/config/schema.ts";
import {
  runSetupWizard,
  SetupWizardError,
  type SetupWizardIo,
  type WizardGitHubClient,
  type WizardInstallation,
} from "../../src/config/setup-wizard.ts";

/**
 * Build a `readLine` that drains a scripted list of answers in order.
 *
 * Returns `null` once the script is exhausted so the wizard sees an EOF
 * if it asks more questions than the test supplied.
 */
function scriptedReader(answers: readonly string[]): {
  readLine: () => Promise<string | null>;
  remaining: () => readonly string[];
} {
  const queue = [...answers];
  return {
    readLine: () => Promise.resolve(queue.shift() ?? null),
    remaining: () => [...queue],
  };
}

/** Capture every line the wizard writes for assertion purposes. */
function captureWriter(): {
  writeLine: (text: string) => void;
  output: () => string;
} {
  const lines: string[] = [];
  return {
    writeLine: (text: string) => {
      lines.push(text);
    },
    output: () => lines.join("\n"),
  };
}

/**
 * Stub {@link WizardGitHubClient} that returns a fixed installations
 * list and records every call. Mirrors the in-memory-doubles convention
 * from W1 but stays here because the wizard's narrow client interface
 * is local to this module.
 */
class StubWizardGitHubClient implements WizardGitHubClient {
  private readonly script: readonly WizardInstallation[];
  readonly calls: { appId: number; privateKeyPath: string }[] = [];

  constructor(script: readonly WizardInstallation[]) {
    this.script = script;
  }

  getInstallations(args: {
    readonly appId: number;
    readonly privateKeyPath: string;
  }): Promise<readonly WizardInstallation[]> {
    this.calls.push({ ...args });
    return Promise.resolve(this.script);
  }
}

class FailingWizardGitHubClient implements WizardGitHubClient {
  // deno-lint-ignore require-await
  async getInstallations(): Promise<readonly WizardInstallation[]> {
    throw new Error("simulated network failure");
  }
}

/**
 * Write a temp file and return its path. The caller is responsible for
 * removal; the helper exists so the happy-path test can hand the
 * wizard a real on-disk private key without leaking files into the
 * repo.
 */
async function makeKeyFile(): Promise<string> {
  const path = await Deno.makeTempFile({
    prefix: "makina-key-",
    suffix: ".pem",
  });
  await Deno.writeTextFile(path, "-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----\n");
  return path;
}

/**
 * Mint an `io` bundle from primitives. Centralises the construction so
 * the per-test setups stay focused on their scripted answers.
 */
function buildIo(args: {
  readonly answers: readonly string[];
  readonly client: WizardGitHubClient;
}): SetupWizardIo & { output: () => string; remaining: () => readonly string[] } {
  const reader = scriptedReader(args.answers);
  const writer = captureWriter();
  return {
    readLine: reader.readLine,
    writeLine: writer.writeLine,
    githubClient: args.client,
    output: writer.output,
    remaining: reader.remaining,
  };
}

Deno.test("runSetupWizard: happy path produces a valid config", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      {
        installationId: 9876543,
        repositories: ["koraytaylan/makina", "koraytaylan/other"],
      },
    ]);
    const io = buildIo({
      answers: ["1234567", keyPath, "2"],
      client,
    });
    const config = await runSetupWizard(io);

    assertEquals(config.github.appId, 1234567);
    assertEquals(config.github.privateKeyPath, keyPath);
    assertEquals(config.github.defaultRepo, "koraytaylan/other");
    assertEquals(config.github.installations["koraytaylan/makina"], 9876543);
    assertEquals(config.github.installations["koraytaylan/other"], 9876543);

    // The wizard must hand off something the loader would also accept.
    const reparsed = parseConfig(JSON.parse(JSON.stringify(config)));
    assertEquals(reparsed.success, true);

    // The wizard called the GitHub client exactly once with the inputs
    // the user supplied.
    assertEquals(client.calls.length, 1);
    const firstCall = client.calls[0];
    assert(firstCall !== undefined);
    assertEquals(firstCall.appId, 1234567);
    assertEquals(firstCall.privateKeyPath, keyPath);

    // Every scripted answer was consumed.
    assertEquals(io.remaining().length, 0);
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: writes prompts that mention each step", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 1, repositories: ["a/b"] },
    ]);
    const io = buildIo({
      answers: ["1", keyPath, "1"],
      client,
    });
    await runSetupWizard(io);
    const transcript = io.output();
    assert(transcript.includes("App ID"));
    assert(transcript.includes("private key"));
    assert(transcript.includes("Discovering installations"));
    assert(transcript.includes("Reachable repositories"));
    assert(transcript.includes("Default repo"));
    assert(transcript.includes("Done"));
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: rejects a missing private-key file", async () => {
  const tempDir = await Deno.makeTempDir({ prefix: "makina-missing-key-" });
  try {
    const phantom = `${tempDir}/never-saved.pem`;
    const client = new StubWizardGitHubClient([]);
    const io = buildIo({
      answers: ["1234567", phantom],
      client,
    });
    const error = await assertRejects(
      () => runSetupWizard(io),
      SetupWizardError,
    );
    assert(
      error.message.includes(phantom),
      `expected error to mention the missing path; got: ${error.message}`,
    );
    // The wizard short-circuits before touching the GitHub client.
    assertEquals(client.calls.length, 0);
  } finally {
    await Deno.remove(tempDir, { recursive: true });
  }
});

Deno.test("runSetupWizard: rejects a non-positive App ID", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([]);
    const io = buildIo({
      answers: ["0", keyPath],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.includes("App ID"));
    assertEquals(client.calls.length, 0);
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: rejects a non-numeric App ID", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([]);
    const io = buildIo({
      answers: ["abc", keyPath],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.includes("App ID"));
    assertEquals(client.calls.length, 0);
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: rejects an out-of-range default-repo choice", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 1, repositories: ["a/b"] },
    ]);
    const io = buildIo({
      answers: ["1", keyPath, "5"],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.includes("range"));
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: rejects a non-numeric default-repo choice", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 1, repositories: ["a/b"] },
    ]);
    const io = buildIo({
      answers: ["1", keyPath, "first"],
      client,
    });
    await assertRejects(() => runSetupWizard(io), SetupWizardError);
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: errors when the App is not installed anywhere", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([]);
    const io = buildIo({
      answers: ["1234567", keyPath],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.toLowerCase().includes("install"));
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: errors when installations exist but no repositories are reachable", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 1, repositories: [] },
    ]);
    const io = buildIo({
      answers: ["1234567", keyPath],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.toLowerCase().includes("repositor"));
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: surfaces failures from the GitHub client", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new FailingWizardGitHubClient();
    const io = buildIo({
      answers: ["1234567", keyPath],
      client,
    });
    const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
    assert(error.message.includes("simulated network failure"));
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: rejects EOF on stdin", async () => {
  const client = new StubWizardGitHubClient([]);
  const io = buildIo({
    answers: [],
    client,
  });
  const error = await assertRejects(() => runSetupWizard(io), SetupWizardError);
  assert(error.message.includes("end of input"));
});

Deno.test("runSetupWizard: trims whitespace from inputs", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 7, repositories: ["a/b"] },
    ]);
    const io = buildIo({
      answers: ["  1234567  ", `  ${keyPath}  `, "  1  "],
      client,
    });
    const config = await runSetupWizard(io);
    assertEquals(config.github.appId, 1234567);
    assertEquals(config.github.privateKeyPath, keyPath);
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: deduplicates a repo that appears in multiple installations", async () => {
  const keyPath = await makeKeyFile();
  try {
    const client = new StubWizardGitHubClient([
      { installationId: 1, repositories: ["a/b", "a/c"] },
      { installationId: 2, repositories: ["a/b", "a/d"] },
    ]);
    const io = buildIo({
      answers: ["1", keyPath, "3"],
      client,
    });
    const config = await runSetupWizard(io);
    // Three distinct repos; the duplicated `a/b` belongs to the first
    // installation (the wizard's deterministic deduplication policy).
    assertEquals(Object.keys(config.github.installations).length, 3);
    assertEquals(config.github.installations["a/b"], 1);
    assertEquals(config.github.installations["a/d"], 2);
    assertEquals(config.github.defaultRepo, "a/d");
  } finally {
    await Deno.remove(keyPath).catch(() => {});
  }
});

Deno.test("runSetupWizard: expands ~/ for the private-key existence check", async () => {
  // Save the key under a temp $HOME so a `~/...` path expands to it.
  const tempHome = await Deno.makeTempDir({ prefix: "makina-home-key-" });
  try {
    const keyPath = `${tempHome}/key.pem`;
    await Deno.writeTextFile(
      keyPath,
      "-----BEGIN PRIVATE KEY-----\nfake\n-----END PRIVATE KEY-----\n",
    );
    const previousHome = Deno.env.get("HOME");
    Deno.env.set("HOME", tempHome);
    try {
      const client = new StubWizardGitHubClient([
        { installationId: 1, repositories: ["a/b"] },
      ]);
      const io = buildIo({
        answers: ["1", "~/key.pem", "1"],
        client,
      });
      const config = await runSetupWizard(io);
      // The user-typed form is preserved so the config stays portable.
      assertEquals(config.github.privateKeyPath, "~/key.pem");
    } finally {
      if (previousHome === undefined) {
        Deno.env.delete("HOME");
      } else {
        Deno.env.set("HOME", previousHome);
      }
    }
  } finally {
    await Deno.remove(tempHome, { recursive: true });
  }
});
