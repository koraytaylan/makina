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

import { ConfigLoadError, expandHome, loadConfig } from "../../src/config/load.ts";

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
    const previousHome = Deno.env.get("HOME");
    Deno.env.set("HOME", tempDir);
    try {
      const config = await loadConfig("~/config.json");
      assertEquals(config.github.appId, 1234567);
    } finally {
      if (previousHome === undefined) {
        Deno.env.delete("HOME");
      } else {
        Deno.env.set("HOME", previousHome);
      }
    }
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
  const previous = Deno.env.get("HOME");
  Deno.env.set("HOME", "/Users/mock");
  try {
    assertEquals(expandHome("~"), "/Users/mock");
  } finally {
    if (previous === undefined) {
      Deno.env.delete("HOME");
    } else {
      Deno.env.set("HOME", previous);
    }
  }
});

Deno.test("expandHome: leaves non-~ paths untouched", () => {
  assertEquals(expandHome("/abs/path"), "/abs/path");
  assertEquals(expandHome("./rel"), "./rel");
  assertEquals(expandHome("~user/foo"), "~user/foo");
});

Deno.test("expandHome: handles a $HOME with a trailing slash without producing //", () => {
  const previous = Deno.env.get("HOME");
  Deno.env.set("HOME", "/Users/mock/");
  try {
    assertEquals(expandHome("~/foo/bar"), "/Users/mock/foo/bar");
  } finally {
    if (previous === undefined) {
      Deno.env.delete("HOME");
    } else {
      Deno.env.set("HOME", previous);
    }
  }
});

Deno.test("expandHome: throws when ~/ is used without a home directory", () => {
  const previousHome = Deno.env.get("HOME");
  const previousProfile = Deno.env.get("USERPROFILE");
  Deno.env.delete("HOME");
  Deno.env.delete("USERPROFILE");
  try {
    assertThrows(() => expandHome("~/foo"), Error, "cannot expand");
  } finally {
    if (previousHome !== undefined) {
      Deno.env.set("HOME", previousHome);
    }
    if (previousProfile !== undefined) {
      Deno.env.set("USERPROFILE", previousProfile);
    }
  }
});

Deno.test("expandHome: falls back to USERPROFILE when HOME is unset", () => {
  const previousHome = Deno.env.get("HOME");
  const previousProfile = Deno.env.get("USERPROFILE");
  Deno.env.delete("HOME");
  Deno.env.set("USERPROFILE", "/Users/mock");
  try {
    assertEquals(expandHome("~/foo"), "/Users/mock/foo");
  } finally {
    if (previousHome !== undefined) {
      Deno.env.set("HOME", previousHome);
    }
    if (previousProfile === undefined) {
      Deno.env.delete("USERPROFILE");
    } else {
      Deno.env.set("USERPROFILE", previousProfile);
    }
  }
});
