/**
 * Unit tests for `src/config/load.ts`.
 *
 * Covers:
 *   - Happy path against a JSON file: round-trips through
 *     {@link parseConfig} and yields a typed {@link Config}.
 *   - JSONC-tolerance: comments and trailing commas parse cleanly.
 *   - `~/` expansion: a leading `~/` resolves against `$HOME` (the
 *     loader runs the home expansion before reading).
 *   - Failure modes: missing file, malformed JSON, schema-invalid
 *     content. Each surfaces a {@link ConfigLoadError} with the
 *     expected `kind`, and the schema-failure message embeds the
 *     failing zod path.
 */

import { assert, assertEquals, assertRejects, assertThrows } from "@std/assert";

import { ConfigLoadError, type EnvLookup, expandHome, loadConfig } from "../../src/config/load.ts";

/**
 * Build an `EnvLookup` over a fixed map. Tests use this instead of
 * mutating `Deno.env`; the suite runs with `--parallel`, so global env
 * mutation can race across files.
 */
function envLookupFrom(entries: Record<string, string>): EnvLookup {
  const map = new Map(Object.entries(entries));
  return (name) => map.get(name);
}

function fullValidConfigText(): string {
  return JSON.stringify(
    {
      github: {
        appId: 1234567,
        privateKeyPath: "/keys/app.pem",
        installations: { "owner/repo": 9876543 },
        defaultRepo: "owner/repo",
      },
      agent: {
        model: "claude-sonnet-4-6",
        permissionMode: "acceptEdits",
        maxIterationsPerTask: 4,
      },
      lifecycle: {
        mergeMode: "squash",
        settlingWindowMilliseconds: 60_000,
        pollIntervalMilliseconds: 30_000,
        preserveWorktreeOnMerge: false,
      },
      workspace: "/tmp/workspace",
      daemon: { socketPath: "/tmp/daemon.sock", autoStart: true },
      tui: { keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" } },
    },
    null,
    2,
  );
}

async function withTempFile<T>(
  contents: string,
  body: (path: string) => Promise<T>,
): Promise<T> {
  const path = await Deno.makeTempFile({ prefix: "makina-config-", suffix: ".json" });
  try {
    await Deno.writeTextFile(path, contents);
    return await body(path);
  } finally {
    await Deno.remove(path).catch(() => {});
  }
}

Deno.test("loadConfig: happy path returns the parsed config", async () => {
  await withTempFile(fullValidConfigText(), async (path) => {
    const config = await loadConfig(path);
    assertEquals(config.github.appId, 1234567);
    assertEquals(config.github.defaultRepo, "owner/repo");
    assertEquals(config.lifecycle.mergeMode, "squash");
  });
});

Deno.test("loadConfig: tolerates JSONC comments and trailing commas", async () => {
  const jsonc = `// makina config
{
  "github": {
    "appId": 1, // an inline comment
    "privateKeyPath": "/k.pem",
    "installations": { "a/b": 1 },
    "defaultRepo": "a/b",
  },
  /* a block
     comment */
  "agent": { "model": "m", "permissionMode": "acceptEdits" },
  "lifecycle": { "mergeMode": "manual" },
  "workspace": "/w",
  "daemon": { "socketPath": "/s" },
}
`;
  await withTempFile(jsonc, async (path) => {
    const config = await loadConfig(path);
    assertEquals(config.github.appId, 1);
    assertEquals(config.lifecycle.mergeMode, "manual");
  });
});

Deno.test("loadConfig: expands a leading ~/ against $HOME", async () => {
  const tempDir = await Deno.makeTempDir({ prefix: "makina-home-" });
  try {
    const filePath = `${tempDir}/config.json`;
    await Deno.writeTextFile(filePath, fullValidConfigText());
    // Inject a per-test `EnvLookup` rather than mutating `Deno.env` so
    // the suite can run with `deno test --parallel` without racing.
    const config = await loadConfig(
      "~/config.json",
      envLookupFrom({ HOME: tempDir }),
    );
    assertEquals(config.github.appId, 1234567);
  } finally {
    await Deno.remove(tempDir, { recursive: true });
  }
});

Deno.test("loadConfig: missing file raises ConfigLoadError(not-found)", async () => {
  const tempDir = await Deno.makeTempDir({ prefix: "makina-missing-" });
  try {
    const missingPath = `${tempDir}/does-not-exist.json`;
    const error = await assertRejects(
      () => loadConfig(missingPath),
      ConfigLoadError,
    );
    assertEquals(error.kind, "not-found");
    assertEquals(error.resolvedPath, missingPath);
    assert(error.message.includes(missingPath));
  } finally {
    await Deno.remove(tempDir, { recursive: true });
  }
});

Deno.test("loadConfig: invalid JSON raises ConfigLoadError(invalid-json)", async () => {
  await withTempFile("{ this is not json", async (path) => {
    const error = await assertRejects(
      () => loadConfig(path),
      ConfigLoadError,
    );
    assertEquals(error.kind, "invalid-json");
    assertEquals(error.resolvedPath, path);
  });
});

Deno.test("loadConfig: schema failure embeds zod path in the message", async () => {
  const broken = JSON.parse(fullValidConfigText());
  broken.github.appId = -1;
  await withTempFile(JSON.stringify(broken), async (path) => {
    const error = await assertRejects(
      () => loadConfig(path),
      ConfigLoadError,
    );
    assertEquals(error.kind, "invalid-schema");
    assert(error.message.includes("github.appId"));
    assert(error.issues.length > 0);
    const first = error.issues[0];
    assert(first !== undefined);
    assertEquals(first.path[0], "github");
    assertEquals(first.path[1], "appId");
  });
});

Deno.test("loadConfig: schema failure with non-identifier key uses bracket notation", async () => {
  const broken = JSON.parse(fullValidConfigText());
  // Put an installation slug that contains slashes, then break the
  // installation id type so the failing path includes the slug as a
  // record key — the load-error formatter renders it with bracket
  // notation rather than dot notation.
  broken.github.installations = { "owner/repo": "not-a-number" };
  await withTempFile(JSON.stringify(broken), async (path) => {
    const error = await assertRejects(
      () => loadConfig(path),
      ConfigLoadError,
    );
    assertEquals(error.kind, "invalid-schema");
    assert(
      error.message.includes('github.installations["owner/repo"]'),
      `expected bracket-notation rendering; got: ${error.message}`,
    );
  });
});

Deno.test("loadConfig: read-failed surfaces when the path points at a directory", async () => {
  const dir = await Deno.makeTempDir({ prefix: "makina-dir-" });
  try {
    const error = await assertRejects(
      () => loadConfig(dir),
      ConfigLoadError,
    );
    // On macOS / Linux, reading a directory returns IsADirectory or
    // similar; the loader does not specialise on it, so it manifests as
    // `read-failed`.
    assertEquals(error.kind, "read-failed");
    assertEquals(error.resolvedPath, dir);
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("expandHome: returns ~ alone as $HOME", () => {
  assertEquals(
    expandHome("~", envLookupFrom({ HOME: "/Users/mock" })),
    "/Users/mock",
  );
});

Deno.test("expandHome: leaves non-~ paths untouched", () => {
  // No `$HOME` lookup is needed for these inputs; pass an empty env so
  // a future bug that *does* reach for `HOME` would surface as a clear
  // "cannot expand" error.
  const empty = envLookupFrom({});
  assertEquals(expandHome("/abs/path", empty), "/abs/path");
  assertEquals(expandHome("./rel", empty), "./rel");
  assertEquals(expandHome("~user/foo", empty), "~user/foo");
});

Deno.test("expandHome: handles a $HOME with a trailing slash without producing //", () => {
  assertEquals(
    expandHome("~/foo/bar", envLookupFrom({ HOME: "/Users/mock/" })),
    "/Users/mock/foo/bar",
  );
});

Deno.test("expandHome: throws when ~/ is used without $HOME set", () => {
  // Per ADR-008 (Windows out of scope) the loader only honors $HOME.
  assertThrows(
    () => expandHome("~/foo", envLookupFrom({})),
    Error,
    "cannot expand",
  );
});

Deno.test("expandHome: ignores USERPROFILE (ADR-008 — Windows out of scope)", () => {
  // We do NOT promise %USERPROFILE% expansion. Setting it without HOME
  // must still raise so we never accidentally start advertising native
  // Windows behavior we don't actually support.
  assertThrows(
    () => expandHome("~/foo", envLookupFrom({ USERPROFILE: "/Users/mock" })),
    Error,
    "cannot expand",
  );
});
