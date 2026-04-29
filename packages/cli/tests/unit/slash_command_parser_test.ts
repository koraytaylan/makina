/**
 * Unit tests for {@link parseSlashCommand}.
 *
 * The plan's §Slash commands lists every supported invocation; each
 * canonical form has at least one assertion below, plus negative cases
 * for malformed input. Errors surface as a typed
 * {@link SlashCommandParseError} so the test suite can pin the failure
 * category as well as the message.
 */

import { assertEquals, assertThrows } from "@std/assert";

import {
  parseSlashCommand,
  SLASH_COMMAND_NAMES,
  SLASH_COMMAND_SPECS,
  SlashCommandParseError,
} from "../../src/tui/slash-command-parser.ts";
import { makeIssueNumber, makeRepoFullName } from "@makina/core";

// ---------------------------------------------------------------------------
// Recognised commands — happy paths
// ---------------------------------------------------------------------------

Deno.test("parseSlashCommand: /issue with positional issue number", () => {
  const payload = parseSlashCommand("/issue 42");
  assertEquals(payload.name, "issue");
  assertEquals(payload.args, ["42"]);
  assertEquals(payload.issueNumber, makeIssueNumber(42));
  assertEquals(payload.repo, undefined);
});

Deno.test("parseSlashCommand: /issue trims surrounding whitespace and trailing newline", () => {
  const payload = parseSlashCommand("  /issue 7  \n");
  assertEquals(payload.name, "issue");
  assertEquals(payload.args, ["7"]);
  assertEquals(payload.issueNumber, makeIssueNumber(7));
});

Deno.test("parseSlashCommand: /issue with --repo flag after the number", () => {
  const payload = parseSlashCommand("/issue 99 --repo owner/repo");
  assertEquals(payload.name, "issue");
  assertEquals(payload.args, ["99"]);
  assertEquals(payload.issueNumber, makeIssueNumber(99));
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

Deno.test("parseSlashCommand: /issue with --repo flag before the number", () => {
  const payload = parseSlashCommand("/issue --repo owner/repo 12");
  assertEquals(payload.name, "issue");
  assertEquals(payload.args, ["12"]);
  assertEquals(payload.issueNumber, makeIssueNumber(12));
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

Deno.test("parseSlashCommand: /issue with --merge=<mode> populates mergeMode", () => {
  // The daemon's handler reads `payload.mergeMode` to forward the
  // operator's intent to `supervisor.start({ ..., mergeMode })`. The
  // parser is the contract surface for that field — see the regression
  // test in `handlers_test.ts` for the wiring side.
  const payload = parseSlashCommand("/issue 7 --merge=squash");
  assertEquals(payload.name, "issue");
  assertEquals(payload.args, ["7"]);
  assertEquals(payload.issueNumber, makeIssueNumber(7));
  assertEquals(payload.mergeMode, "squash");
});

Deno.test("parseSlashCommand: /issue accepts --merge <mode> as two tokens", () => {
  // Equivalent spelling for parity with `--repo <owner/name>`.
  const payload = parseSlashCommand("/issue 8 --merge rebase");
  assertEquals(payload.mergeMode, "rebase");
});

Deno.test("parseSlashCommand: /issue --merge accepts manual mode", () => {
  const payload = parseSlashCommand("/issue 9 --merge=manual");
  assertEquals(payload.mergeMode, "manual");
});

Deno.test("parseSlashCommand: /issue without --merge leaves mergeMode undefined", () => {
  // The supervisor's default kicks in only when the parser does not
  // populate the field — make sure the omission is faithful.
  const payload = parseSlashCommand("/issue 10");
  assertEquals(payload.mergeMode, undefined);
});

Deno.test('parseSlashCommand: /issue with an unknown --merge value throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --merge=ff-only"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /issue with --merge missing its value throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --merge"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /issue with empty --merge= value throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --merge="),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /issue rejects duplicate --merge flags", () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --merge=squash --merge=rebase"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo add returns the repo + installation id tokens", () => {
  const payload = parseSlashCommand("/repo add owner/repo 9876543");
  assertEquals(payload.name, "repo");
  assertEquals(payload.args, ["add", "owner/repo", "9876543"]);
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

Deno.test("parseSlashCommand: /repo default forwards the repo token", () => {
  const payload = parseSlashCommand("/repo default owner/repo");
  assertEquals(payload.name, "repo");
  assertEquals(payload.args, ["default", "owner/repo"]);
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

Deno.test("parseSlashCommand: /repo list takes no arguments", () => {
  const payload = parseSlashCommand("/repo list");
  assertEquals(payload.name, "repo");
  assertEquals(payload.args, ["list"]);
  assertEquals(payload.repo, undefined);
});

Deno.test("parseSlashCommand: /status returns name with empty args", () => {
  const payload = parseSlashCommand("/status");
  assertEquals(payload.name, "status");
  assertEquals(payload.args, []);
});

Deno.test("parseSlashCommand: /switch forwards the task-id token", () => {
  const payload = parseSlashCommand("/switch task_2026-04-26T12-00-00_abc123");
  assertEquals(payload.name, "switch");
  assertEquals(payload.args, ["task_2026-04-26T12-00-00_abc123"]);
});

Deno.test("parseSlashCommand: /cancel forwards the task-id token", () => {
  const payload = parseSlashCommand("/cancel task_x");
  assertEquals(payload.name, "cancel");
  assertEquals(payload.args, ["task_x"]);
});

Deno.test("parseSlashCommand: /retry forwards the task-id token", () => {
  const payload = parseSlashCommand("/retry task_y");
  assertEquals(payload.name, "retry");
  assertEquals(payload.args, ["task_y"]);
});

Deno.test("parseSlashCommand: /merge forwards the task-id token", () => {
  const payload = parseSlashCommand("/merge task_z");
  assertEquals(payload.name, "merge");
  assertEquals(payload.args, ["task_z"]);
});

Deno.test("parseSlashCommand: /logs forwards the task-id token", () => {
  const payload = parseSlashCommand("/logs task_q");
  assertEquals(payload.name, "logs");
  assertEquals(payload.args, ["task_q"]);
});

Deno.test("parseSlashCommand: /quit takes no arguments", () => {
  const payload = parseSlashCommand("/quit");
  assertEquals(payload.name, "quit");
  assertEquals(payload.args, []);
});

Deno.test("parseSlashCommand: /daemon stop is the only daemon sub-command", () => {
  const payload = parseSlashCommand("/daemon stop");
  assertEquals(payload.name, "daemon");
  assertEquals(payload.args, ["stop"]);
});

Deno.test("parseSlashCommand: /help with no argument lists every command", () => {
  const payload = parseSlashCommand("/help");
  assertEquals(payload.name, "help");
  assertEquals(payload.args, []);
});

Deno.test("parseSlashCommand: /help with a target forwards the argument", () => {
  const payload = parseSlashCommand("/help issue");
  assertEquals(payload.name, "help");
  assertEquals(payload.args, ["issue"]);
});

// ---------------------------------------------------------------------------
// Whitespace and tokenisation edge cases
// ---------------------------------------------------------------------------

Deno.test("parseSlashCommand: collapses runs of whitespace between tokens", () => {
  const payload = parseSlashCommand("/issue   42    --repo   owner/repo");
  assertEquals(payload.args, ["42"]);
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

Deno.test("parseSlashCommand: tabs separate tokens like spaces", () => {
  const payload = parseSlashCommand("/issue\t42\t--repo\towner/repo");
  assertEquals(payload.args, ["42"]);
  assertEquals(payload.repo, makeRepoFullName("owner/repo"));
});

// ---------------------------------------------------------------------------
// Negative cases — every error category exercised
// ---------------------------------------------------------------------------

Deno.test('parseSlashCommand: empty input throws "empty"', () => {
  const error = assertThrows(
    () => parseSlashCommand(""),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "empty");
});

Deno.test('parseSlashCommand: lone slash throws "empty"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "empty");
});

Deno.test('parseSlashCommand: input without a leading slash throws "not-a-command"', () => {
  const error = assertThrows(
    () => parseSlashCommand("issue 42"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "not-a-command");
});

Deno.test('parseSlashCommand: unrecognised command throws "unknown-command"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/sneeze"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "unknown-command");
});

Deno.test('parseSlashCommand: /issue without a number throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /issue with a non-numeric arg throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue notanumber"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /issue with --repo missing its value throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --repo"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /issue rejects duplicate --repo flags", () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --repo a/b --repo c/d"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /issue rejects unknown flags", () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --boom"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /issue rejects extra positional arguments", () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 2"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /issue rejects malformed --repo value", () => {
  const error = assertThrows(
    () => parseSlashCommand("/issue 1 --repo nope"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /repo without sub-command throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /repo with unknown sub-command throws "bad-arguments"', () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo install"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test('parseSlashCommand: /repo add without all args throws "bad-arguments"', () => {
  const e1 = assertThrows(
    () => parseSlashCommand("/repo add"),
    SlashCommandParseError,
  );
  assertEquals(e1.kind, "bad-arguments");
  const e2 = assertThrows(
    () => parseSlashCommand("/repo add owner/repo"),
    SlashCommandParseError,
  );
  assertEquals(e2.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo add with non-numeric installation id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo add owner/repo abc"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo add with extra args throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo add owner/repo 1 2"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo default rejects malformed repo arg", () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo default not-a-slash"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo default rejects extra args", () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo default owner/repo extra"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /repo list rejects extra args", () => {
  const error = assertThrows(
    () => parseSlashCommand("/repo list now"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /status rejects extra args", () => {
  const error = assertThrows(
    () => parseSlashCommand("/status all"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /quit rejects extra args", () => {
  const error = assertThrows(
    () => parseSlashCommand("/quit now"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /switch without task id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/switch"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /switch with extra args throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/switch t1 t2"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /cancel without task id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/cancel"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /retry without task id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/retry"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /merge without task id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/merge"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /logs without task id throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/logs"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /daemon without sub-command throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/daemon"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /daemon with unknown sub-command throws", () => {
  const error = assertThrows(
    () => parseSlashCommand("/daemon restart"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /daemon stop rejects extra args", () => {
  const error = assertThrows(
    () => parseSlashCommand("/daemon stop now"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

Deno.test("parseSlashCommand: /help rejects more than one argument", () => {
  const error = assertThrows(
    () => parseSlashCommand("/help issue repo"),
    SlashCommandParseError,
  );
  assertEquals(error.kind, "bad-arguments");
});

// ---------------------------------------------------------------------------
// Spec metadata invariants
// ---------------------------------------------------------------------------

Deno.test("SLASH_COMMAND_SPECS: every name from SLASH_COMMAND_NAMES has a spec", () => {
  for (const name of SLASH_COMMAND_NAMES) {
    const spec = SLASH_COMMAND_SPECS.find((entry) => entry.name === name);
    if (spec === undefined) {
      throw new Error(`spec missing for "${name}"`);
    }
    assertEquals(spec.name, name);
  }
  assertEquals(SLASH_COMMAND_SPECS.length, SLASH_COMMAND_NAMES.length);
});

Deno.test("SLASH_COMMAND_SPECS: usage strings begin with /<name>", () => {
  for (const spec of SLASH_COMMAND_SPECS) {
    if (!spec.usage.startsWith(`/${spec.name}`)) {
      throw new Error(
        `usage for "${spec.name}" does not start with the expected prefix: ${spec.usage}`,
      );
    }
  }
});

Deno.test("SLASH_COMMAND_SPECS: alphabetical order", () => {
  const names = SLASH_COMMAND_SPECS.map((entry) => entry.name);
  const sorted = [...names].sort();
  assertEquals(names, sorted);
});
