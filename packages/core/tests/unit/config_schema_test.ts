/**
 * Unit tests for `src/config/schema.ts`. Cover:
 *
 *  - Happy path: a fully-populated config parses, returning the inferred
 *    `Config` shape.
 *  - Defaults: omitted optional fields take their documented defaults.
 *  - Field-level rejection: each bounded field rejects out-of-range
 *    values, and the failing zod path is the field path itself.
 *  - Cross-field invariant: `defaultRepo` must be present in
 *    `installations`.
 *  - Missing required fields are rejected.
 *
 * The loader (Wave 2) handles `~/` expansion; the schema does not, and
 * this file does not assert on path expansion.
 */

import { assert, assertEquals } from "@std/assert";

import { type Config, parseConfig } from "../../src/config/schema.ts";
import {
  MAX_MAX_TASK_ITERATIONS,
  MAX_POLL_INTERVAL_MILLISECONDS,
  MAX_SETTLING_WINDOW_MILLISECONDS,
  MAX_TASK_ITERATIONS,
  MIN_GITHUB_APP_ID,
  MIN_MAX_TASK_ITERATIONS,
  MIN_POLL_INTERVAL_MILLISECONDS,
  MIN_SETTLING_WINDOW_MILLISECONDS,
  POLL_INTERVAL_MILLISECONDS,
  SETTLING_WINDOW_MILLISECONDS,
} from "../../src/constants.ts";

function fullValidConfig(): unknown {
  return {
    github: {
      appId: 1234567,
      privateKeyPath: "~/.config/makina/key.pem",
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
      settlingWindowMilliseconds: SETTLING_WINDOW_MILLISECONDS,
      pollIntervalMilliseconds: POLL_INTERVAL_MILLISECONDS,
      preserveWorktreeOnMerge: false,
    },
    workspace: "~/Library/Application Support/makina/workspace",
    daemon: {
      socketPath: "~/Library/Application Support/makina/daemon.sock",
      autoStart: true,
    },
    tui: {
      keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" },
    },
  };
}

function parseOrThrow(raw: unknown): Config {
  const result = parseConfig(raw);
  if (!result.success) {
    throw new Error(`expected parse to succeed; got ${JSON.stringify(result.issues)}`);
  }
  return result.data;
}

function expectIssueAtPath(
  raw: unknown,
  expectedPath: readonly (string | number)[],
): void {
  const result = parseConfig(raw);
  assertEquals(result.success, false);
  if (result.success) {
    throw new Error("expected schema parse to fail");
  }
  const matched = result.issues.some((issue) =>
    issue.path.length === expectedPath.length &&
    issue.path.every((segment, index) => segment === expectedPath[index])
  );
  assert(
    matched,
    `expected an issue at path ${JSON.stringify(expectedPath)}; saw ${
      JSON.stringify(result.issues.map((issue) => issue.path))
    }`,
  );
}

Deno.test("parseConfig: happy path returns the inferred Config type", () => {
  const config: Config = parseOrThrow(fullValidConfig());
  assertEquals(config.github.appId, 1234567);
  assertEquals(config.agent.permissionMode, "acceptEdits");
  assertEquals(config.lifecycle.mergeMode, "squash");
  assertEquals(config.tui.keybindings.commandPalette, "ctrl+p");
});

Deno.test("parseConfig: defaults fill in optional fields", () => {
  const minimal: unknown = {
    github: {
      appId: 1,
      privateKeyPath: "/k.pem",
      installations: { "a/b": 1 },
      defaultRepo: "a/b",
    },
    agent: { model: "m", permissionMode: "acceptEdits" },
    lifecycle: { mergeMode: "manual" },
    workspace: "/w",
    daemon: { socketPath: "/s" },
  };
  const config = parseOrThrow(minimal);
  assertEquals(config.agent.maxIterationsPerTask, MAX_TASK_ITERATIONS);
  assertEquals(config.lifecycle.settlingWindowMilliseconds, SETTLING_WINDOW_MILLISECONDS);
  assertEquals(config.lifecycle.pollIntervalMilliseconds, POLL_INTERVAL_MILLISECONDS);
  assertEquals(config.lifecycle.preserveWorktreeOnMerge, false);
  assertEquals(config.daemon.autoStart, true);
  assertEquals(config.tui.keybindings.commandPalette, "ctrl+p");
  assertEquals(config.tui.keybindings.taskSwitcher, "ctrl+g");
});

Deno.test("parseConfig: rejects an appId below MIN_GITHUB_APP_ID", () => {
  const raw = fullValidConfig() as { github: { appId: number } };
  raw.github.appId = MIN_GITHUB_APP_ID - 1;
  expectIssueAtPath(raw, ["github", "appId"]);
});

Deno.test("parseConfig: rejects a non-integer appId", () => {
  const raw = fullValidConfig() as { github: { appId: number } };
  raw.github.appId = 1.5;
  expectIssueAtPath(raw, ["github", "appId"]);
});

Deno.test("parseConfig: rejects empty privateKeyPath", () => {
  const raw = fullValidConfig() as { github: { privateKeyPath: string } };
  raw.github.privateKeyPath = "";
  expectIssueAtPath(raw, ["github", "privateKeyPath"]);
});

Deno.test("parseConfig: rejects malformed installations key", () => {
  const raw = fullValidConfig() as {
    github: {
      installations: Record<string, number>;
      defaultRepo: string;
    };
  };
  raw.github.installations = { "no-slash": 1 };
  raw.github.defaultRepo = "no-slash";
  // Zod records report failures under the offending key; we only require
  // that *some* issue lands inside the installations branch.
  const result = parseConfig(raw);
  assertEquals(result.success, false);
  if (result.success) {
    return;
  }
  const matched = result.issues.some(
    (issue) => issue.path[0] === "github" && issue.path[1] === "installations",
  );
  assert(matched, JSON.stringify(result.issues));
});

Deno.test("parseConfig: rejects defaultRepo missing from installations", () => {
  const raw = fullValidConfig() as { github: { defaultRepo: string } };
  raw.github.defaultRepo = "ghost/repo";
  expectIssueAtPath(raw, ["github", "defaultRepo"]);
});

Deno.test("parseConfig: rejects an unsupported mergeMode", () => {
  const raw = fullValidConfig() as { lifecycle: { mergeMode: string } };
  raw.lifecycle.mergeMode = "fast-forward";
  expectIssueAtPath(raw, ["lifecycle", "mergeMode"]);
});

Deno.test("parseConfig: rejects out-of-range settlingWindowMilliseconds", () => {
  const raw = fullValidConfig() as {
    lifecycle: { settlingWindowMilliseconds: number };
  };
  raw.lifecycle.settlingWindowMilliseconds = MIN_SETTLING_WINDOW_MILLISECONDS - 1;
  expectIssueAtPath(raw, ["lifecycle", "settlingWindowMilliseconds"]);

  raw.lifecycle.settlingWindowMilliseconds = MAX_SETTLING_WINDOW_MILLISECONDS + 1;
  expectIssueAtPath(raw, ["lifecycle", "settlingWindowMilliseconds"]);
});

Deno.test("parseConfig: rejects out-of-range pollIntervalMilliseconds", () => {
  const raw = fullValidConfig() as {
    lifecycle: { pollIntervalMilliseconds: number };
  };
  raw.lifecycle.pollIntervalMilliseconds = MIN_POLL_INTERVAL_MILLISECONDS - 1;
  expectIssueAtPath(raw, ["lifecycle", "pollIntervalMilliseconds"]);

  raw.lifecycle.pollIntervalMilliseconds = MAX_POLL_INTERVAL_MILLISECONDS + 1;
  expectIssueAtPath(raw, ["lifecycle", "pollIntervalMilliseconds"]);
});

Deno.test("parseConfig: rejects out-of-range maxIterationsPerTask", () => {
  const raw = fullValidConfig() as {
    agent: { maxIterationsPerTask: number };
  };
  raw.agent.maxIterationsPerTask = MIN_MAX_TASK_ITERATIONS - 1;
  expectIssueAtPath(raw, ["agent", "maxIterationsPerTask"]);

  raw.agent.maxIterationsPerTask = MAX_MAX_TASK_ITERATIONS + 1;
  expectIssueAtPath(raw, ["agent", "maxIterationsPerTask"]);
});

Deno.test("parseConfig: rejects unsupported permissionMode", () => {
  const raw = fullValidConfig() as { agent: { permissionMode: string } };
  raw.agent.permissionMode = "ask";
  expectIssueAtPath(raw, ["agent", "permissionMode"]);
});

Deno.test("parseConfig: rejects empty workspace", () => {
  const raw = fullValidConfig() as { workspace: string };
  raw.workspace = "";
  expectIssueAtPath(raw, ["workspace"]);
});

Deno.test("parseConfig: rejects empty socketPath", () => {
  const raw = fullValidConfig() as { daemon: { socketPath: string } };
  raw.daemon.socketPath = "";
  expectIssueAtPath(raw, ["daemon", "socketPath"]);
});

Deno.test("parseConfig: rejects missing top-level fields", () => {
  // Each missing field surfaces at its own zod path.
  const cases: { remove: keyof Config; expectedPath: readonly string[] }[] = [
    { remove: "github", expectedPath: ["github"] },
    { remove: "agent", expectedPath: ["agent"] },
    { remove: "lifecycle", expectedPath: ["lifecycle"] },
    { remove: "workspace", expectedPath: ["workspace"] },
    { remove: "daemon", expectedPath: ["daemon"] },
  ];
  for (const { remove, expectedPath } of cases) {
    const raw = fullValidConfig() as Record<string, unknown>;
    delete raw[remove];
    expectIssueAtPath(raw, expectedPath);
  }
});

Deno.test("parseConfig: rejects empty model string", () => {
  const raw = fullValidConfig() as { agent: { model: string } };
  raw.agent.model = "";
  expectIssueAtPath(raw, ["agent", "model"]);
});

Deno.test("parseConfig: errors carry helpful field paths", () => {
  const raw = fullValidConfig() as { github: { appId: number } };
  raw.github.appId = -1;
  const result = parseConfig(raw);
  assertEquals(result.success, false);
  if (result.success) {
    return;
  }
  const issue = result.issues[0];
  assert(issue !== undefined);
  assertEquals(issue.path, ["github", "appId"]);
});
