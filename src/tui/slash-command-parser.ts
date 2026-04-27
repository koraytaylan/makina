/**
 * tui/slash-command-parser.ts — pure parser for the TUI's slash-command
 * vocabulary.
 *
 * The TUI's command palette lets the user type a leading-`/` line and
 * dispatches it to the daemon as a `command` envelope. This module is
 * the single source of truth for what a valid command looks like:
 * tokenises the line, applies per-command argument grammar, returns a
 * {@link CommandPayload} ready to drop into a {@link MessageEnvelope},
 * and rejects malformed input through {@link SlashCommandParseError}.
 *
 * The parser is deliberately transport-agnostic — it does not call the
 * daemon, does not touch React state, and has no side-effects. The
 * command palette wires the result of {@link parseSlashCommand} into the
 * IPC client; the unit tests exercise the parser in isolation.
 *
 * **Supported commands** (full grammar in {@link SLASH_COMMAND_SPECS}):
 *
 * - `/issue <number> [--repo <owner/name>]` — start a new task.
 * - `/repo add <owner/name> <installation-id>` — register a repo.
 * - `/repo default <owner/name>` — set the default repo.
 * - `/repo list` — print the registered repositories.
 * - `/status` — print every in-flight task's state.
 * - `/switch <task-id>` — focus a task.
 * - `/cancel <task-id>` — cancel a task; preserves the worktree.
 * - `/retry <task-id>` — re-enter a `NEEDS_HUMAN` task.
 * - `/merge <task-id>` — force the merge of a `READY_TO_MERGE` task.
 * - `/logs <task-id>` — open the task's scrollback.
 * - `/quit` — exit the TUI (the daemon keeps running).
 * - `/daemon stop` — stop the daemon process.
 * - `/help [command]` — list commands or describe one.
 *
 * @module
 */

import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type RepoFullName,
} from "../types.ts";
import type { CommandPayload } from "../ipc/protocol.ts";
import { RADIX_DECIMAL } from "../constants.ts";

// ---------------------------------------------------------------------------
// Public type contracts
// ---------------------------------------------------------------------------

/**
 * Names of every slash command the parser recognises.
 *
 * Exported as a `readonly` tuple so consumers (the command-palette
 * autocomplete, the help command) can iterate it.
 */
export const SLASH_COMMAND_NAMES = [
  "issue",
  "repo",
  "status",
  "switch",
  "cancel",
  "retry",
  "merge",
  "logs",
  "quit",
  "daemon",
  "help",
] as const;

/**
 * Discriminant union of valid command names.
 */
export type SlashCommandName = typeof SLASH_COMMAND_NAMES[number];

/**
 * One row in the spec table the parser drives off. Entries describe the
 * canonical command shape so the help text and the autocomplete
 * dropdown stay in sync with the parser.
 */
export interface SlashCommandSpec {
  /** The command name, without the leading `/`. */
  readonly name: SlashCommandName;
  /** One-line summary surfaced by `/help` and the autocomplete dropdown. */
  readonly summary: string;
  /** Canonical usage string surfaced by `/help <command>`. */
  readonly usage: string;
}

/**
 * Static specs for every command. Sorted alphabetically so the
 * autocomplete dropdown's order is deterministic.
 */
export const SLASH_COMMAND_SPECS: readonly SlashCommandSpec[] = [
  {
    name: "cancel",
    summary: "Cancel a non-terminal task; preserves the worktree.",
    usage: "/cancel <task-id>",
  },
  {
    name: "daemon",
    summary: "Manage the long-running daemon (sub-commands: stop).",
    usage: "/daemon stop",
  },
  {
    name: "help",
    summary: "List commands or describe one.",
    usage: "/help [command]",
  },
  {
    name: "issue",
    summary: "Start a new task for an issue.",
    usage: "/issue <number> [--repo <owner/name>]",
  },
  {
    name: "logs",
    summary: "Open the task scrollback.",
    usage: "/logs <task-id>",
  },
  {
    name: "merge",
    summary: "Force the merge of a READY_TO_MERGE task.",
    usage: "/merge <task-id>",
  },
  {
    name: "quit",
    summary: "Exit the TUI; the daemon keeps running.",
    usage: "/quit",
  },
  {
    name: "repo",
    summary: "Manage registered repositories (add/default/list).",
    usage: "/repo {add <owner/name> <installation-id> | default <owner/name> | list}",
  },
  {
    name: "retry",
    summary: "Re-enter a NEEDS_HUMAN task after a manual fix.",
    usage: "/retry <task-id>",
  },
  {
    name: "status",
    summary: "Print every in-flight task's state.",
    usage: "/status",
  },
  {
    name: "switch",
    summary: "Focus the listed task.",
    usage: "/switch <task-id>",
  },
];

/**
 * Error raised by {@link parseSlashCommand} for any malformed input.
 *
 * The message is suitable for direct rendering in the command-palette
 * status row; the {@link SlashCommandParseError.kind} tag distinguishes
 * cases the renderer wants to highlight differently (an unknown command
 * is autocompletable; a malformed argument is not).
 */
export class SlashCommandParseError extends Error {
  /**
   * Tag describing the failure category.
   *
   * - `not-a-command` — input did not start with `/`.
   * - `empty` — input was just `/` or whitespace.
   * - `unknown-command` — the command name is not in
   *   {@link SLASH_COMMAND_NAMES}.
   * - `bad-arguments` — the command was recognised but its arguments
   *   failed validation.
   */
  readonly kind:
    | "not-a-command"
    | "empty"
    | "unknown-command"
    | "bad-arguments";

  /**
   * Construct a parse error.
   *
   * @param kind Category tag (see {@link SlashCommandParseError.kind}).
   * @param message Human-readable description for the status row.
   */
  constructor(
    kind:
      | "not-a-command"
      | "empty"
      | "unknown-command"
      | "bad-arguments",
    message: string,
  ) {
    super(message);
    this.kind = kind;
    this.name = "SlashCommandParseError";
  }
}

// ---------------------------------------------------------------------------
// Public parse API
// ---------------------------------------------------------------------------

/**
 * Parse a single line of palette input into a {@link CommandPayload}.
 *
 * Whitespace surrounding the line is trimmed; tokens within the line
 * are split on runs of ASCII whitespace (no shell-style quoting — the
 * grammar does not need it). The returned payload is ready to drop
 * into a `command` envelope.
 *
 * @param input The raw user input (with or without trailing newline).
 * @returns The parsed command payload.
 * @throws {SlashCommandParseError} If the input is not a slash command,
 *   the command is unknown, or its arguments are malformed.
 *
 * @example
 * ```ts
 * const payload = parseSlashCommand("/issue 42 --repo owner/repo");
 * // → { name: "issue", args: ["42"], issueNumber: 42, repo: "owner/repo" }
 * ```
 */
export function parseSlashCommand(input: string): CommandPayload {
  const trimmed = input.trim();
  if (trimmed.length === 0 || trimmed === "/") {
    throw new SlashCommandParseError("empty", "Type a command after the slash.");
  }
  if (!trimmed.startsWith("/")) {
    throw new SlashCommandParseError(
      "not-a-command",
      `Commands must start with "/"; got ${JSON.stringify(input)}.`,
    );
  }
  const tokens = tokenise(trimmed.slice(1));
  const head = tokens.shift();
  if (head === undefined || head.length === 0) {
    throw new SlashCommandParseError("empty", "Type a command after the slash.");
  }
  if (!isKnownCommand(head)) {
    throw new SlashCommandParseError(
      "unknown-command",
      `Unknown command "/${head}". Type /help for a list.`,
    );
  }
  switch (head) {
    case "issue":
      return parseIssue(tokens);
    case "repo":
      return parseRepo(tokens);
    case "status":
      return parseNoArgs("status", tokens);
    case "switch":
      return parseSingleTaskId("switch", tokens);
    case "cancel":
      return parseSingleTaskId("cancel", tokens);
    case "retry":
      return parseSingleTaskId("retry", tokens);
    case "merge":
      return parseSingleTaskId("merge", tokens);
    case "logs":
      return parseSingleTaskId("logs", tokens);
    case "quit":
      return parseNoArgs("quit", tokens);
    case "daemon":
      return parseDaemon(tokens);
    case "help":
      return parseHelp(tokens);
  }
}

/**
 * Type guard backing the dispatch in {@link parseSlashCommand}.
 *
 * @param raw Candidate command name (without the leading `/`).
 * @returns `true` when `raw` is one of {@link SLASH_COMMAND_NAMES}.
 */
function isKnownCommand(raw: string): raw is SlashCommandName {
  return (SLASH_COMMAND_NAMES as readonly string[]).includes(raw);
}

// ---------------------------------------------------------------------------
// Per-command grammar
// ---------------------------------------------------------------------------

/**
 * Parse `/issue <number> [--repo <owner/name>]`.
 *
 * The `--repo` flag is optional and may appear before or after the
 * positional issue number; positional non-flag tokens beyond the issue
 * number are rejected so the parser fails fast on typos.
 *
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseIssue(tokens: readonly string[]): CommandPayload {
  let issueArg: string | undefined;
  let repoArg: string | undefined;
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token === "--repo") {
      const next = tokens[index + 1];
      if (next === undefined) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/issue: --repo requires an argument.",
        );
      }
      if (repoArg !== undefined) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/issue: --repo specified more than once.",
        );
      }
      repoArg = next;
      index += 1;
      continue;
    }
    if (token !== undefined && token.startsWith("--")) {
      throw new SlashCommandParseError(
        "bad-arguments",
        `/issue: unknown flag ${JSON.stringify(token)}.`,
      );
    }
    if (issueArg !== undefined) {
      throw new SlashCommandParseError(
        "bad-arguments",
        "/issue accepts a single issue number.",
      );
    }
    issueArg = token;
  }
  if (issueArg === undefined) {
    throw new SlashCommandParseError(
      "bad-arguments",
      "/issue requires an issue number.",
    );
  }
  const issueNumber = parseIssueNumberArg("/issue", issueArg);
  const repo = repoArg === undefined ? undefined : parseRepoFullNameArg("/issue", repoArg);
  return makePayload({
    name: "issue",
    args: [issueArg],
    issueNumber,
    repo,
  });
}

/**
 * Parse `/repo {add <owner/name> <installation-id> | default <owner/name>
 * | list}`.
 *
 * The sub-command is required; unknown sub-commands raise
 * `bad-arguments`.
 *
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseRepo(tokens: readonly string[]): CommandPayload {
  const subcommand = tokens[0];
  if (subcommand === undefined) {
    throw new SlashCommandParseError(
      "bad-arguments",
      "/repo requires a sub-command (add | default | list).",
    );
  }
  switch (subcommand) {
    case "add": {
      const repoToken = tokens[1];
      const installationToken = tokens[2];
      if (repoToken === undefined) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo add requires <owner/name>.",
        );
      }
      if (installationToken === undefined) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo add requires <installation-id>.",
        );
      }
      if (tokens.length > 3) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo add accepts exactly two arguments.",
        );
      }
      const repo = parseRepoFullNameArg("/repo add", repoToken);
      // Validate the installation id parses as a positive integer up
      // front. The daemon will re-validate, but a fast local check
      // gives the user a tight error loop.
      const installationId = Number.parseInt(installationToken, RADIX_DECIMAL);
      if (
        !Number.isFinite(installationId) ||
        !Number.isInteger(installationId) ||
        installationId <= 0
      ) {
        throw new SlashCommandParseError(
          "bad-arguments",
          `/repo add: installation id must be a positive integer; got ${
            JSON.stringify(installationToken)
          }.`,
        );
      }
      return makePayload({
        name: "repo",
        args: ["add", repoToken, installationToken],
        repo,
      });
    }
    case "default": {
      const repoToken = tokens[1];
      if (repoToken === undefined) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo default requires <owner/name>.",
        );
      }
      if (tokens.length > 2) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo default accepts exactly one argument.",
        );
      }
      const repo = parseRepoFullNameArg("/repo default", repoToken);
      return makePayload({
        name: "repo",
        args: ["default", repoToken],
        repo,
      });
    }
    case "list": {
      if (tokens.length > 1) {
        throw new SlashCommandParseError(
          "bad-arguments",
          "/repo list accepts no arguments.",
        );
      }
      return makePayload({ name: "repo", args: ["list"] });
    }
    default:
      throw new SlashCommandParseError(
        "bad-arguments",
        `/repo: unknown sub-command ${JSON.stringify(subcommand)}; expected add, default, or list.`,
      );
  }
}

/**
 * Parse a command that takes no arguments (`/status`, `/quit`).
 *
 * @param name The command name.
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseNoArgs(
  name: "status" | "quit",
  tokens: readonly string[],
): CommandPayload {
  if (tokens.length > 0) {
    throw new SlashCommandParseError(
      "bad-arguments",
      `/${name} accepts no arguments.`,
    );
  }
  return makePayload({ name, args: [] });
}

/**
 * Parse a command that takes a single task-id argument (`/switch`,
 * `/cancel`, `/retry`, `/merge`, `/logs`).
 *
 * The task-id is forwarded as a string in `args[0]`; the daemon owns
 * the shape validation against its in-memory task table.
 *
 * @param name The command name.
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseSingleTaskId(
  name: "switch" | "cancel" | "retry" | "merge" | "logs",
  tokens: readonly string[],
): CommandPayload {
  const taskIdToken = tokens[0];
  if (taskIdToken === undefined || taskIdToken.length === 0) {
    throw new SlashCommandParseError(
      "bad-arguments",
      `/${name} requires <task-id>.`,
    );
  }
  if (tokens.length > 1) {
    throw new SlashCommandParseError(
      "bad-arguments",
      `/${name} accepts exactly one argument.`,
    );
  }
  return makePayload({ name, args: [taskIdToken] });
}

/**
 * Parse `/daemon stop`. No other sub-commands exist yet; unknown
 * sub-commands raise `bad-arguments`.
 *
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseDaemon(tokens: readonly string[]): CommandPayload {
  const subcommand = tokens[0];
  if (subcommand === undefined) {
    throw new SlashCommandParseError(
      "bad-arguments",
      "/daemon requires a sub-command (stop).",
    );
  }
  if (subcommand !== "stop") {
    throw new SlashCommandParseError(
      "bad-arguments",
      `/daemon: unknown sub-command ${JSON.stringify(subcommand)}; expected stop.`,
    );
  }
  if (tokens.length > 1) {
    throw new SlashCommandParseError(
      "bad-arguments",
      "/daemon stop accepts no arguments.",
    );
  }
  return makePayload({ name: "daemon", args: ["stop"] });
}

/**
 * Parse `/help [command]`. The optional argument constrains the help
 * output to a single command.
 *
 * @param tokens Tokens after the command name.
 * @returns The command payload.
 */
function parseHelp(tokens: readonly string[]): CommandPayload {
  if (tokens.length === 0) {
    return makePayload({ name: "help", args: [] });
  }
  if (tokens.length > 1) {
    throw new SlashCommandParseError(
      "bad-arguments",
      "/help accepts at most one argument.",
    );
  }
  const target = tokens[0];
  if (target === undefined) {
    return makePayload({ name: "help", args: [] });
  }
  return makePayload({ name: "help", args: [target] });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Split the post-slash text into whitespace-separated tokens.
 *
 * Empty tokens (runs of whitespace) are dropped so callers do not have
 * to special-case them in their grammar checks.
 *
 * @param raw The post-slash text.
 * @returns The token array.
 */
function tokenise(raw: string): string[] {
  return raw.split(/\s+/u).filter((part) => part.length > 0);
}

/**
 * Parse a token expected to be a positive integer issue number.
 *
 * @param scope Caller name, prepended to the error message.
 * @param raw The candidate token.
 * @returns The branded {@link IssueNumber}.
 */
function parseIssueNumberArg(scope: string, raw: string): IssueNumber {
  const numeric = Number.parseInt(raw, RADIX_DECIMAL);
  if (!Number.isFinite(numeric)) {
    throw new SlashCommandParseError(
      "bad-arguments",
      `${scope}: issue number must be a positive integer; got ${JSON.stringify(raw)}.`,
    );
  }
  try {
    return makeIssueNumber(numeric);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new SlashCommandParseError(
      "bad-arguments",
      `${scope}: ${message}.`,
    );
  }
}

/**
 * Parse a token expected to be `<owner>/<name>`.
 *
 * @param scope Caller name, prepended to the error message.
 * @param raw The candidate token.
 * @returns The branded {@link RepoFullName}.
 */
function parseRepoFullNameArg(scope: string, raw: string): RepoFullName {
  try {
    return makeRepoFullName(raw);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new SlashCommandParseError(
      "bad-arguments",
      `${scope}: ${message}.`,
    );
  }
}

/**
 * Build a {@link CommandPayload}, leaving optional fields undefined
 * (vs. setting them to `undefined`) so callers downstream that read the
 * value via `?.` or rest-spread see the expected `Object.keys` set.
 *
 * @param input Per-command pieces.
 * @returns The constructed payload.
 */
function makePayload(
  input: {
    readonly name: SlashCommandName;
    readonly args: readonly string[];
    readonly repo?: RepoFullName | undefined;
    readonly issueNumber?: IssueNumber | undefined;
  },
): CommandPayload {
  const payload: {
    name: SlashCommandName;
    args: readonly string[];
    repo?: RepoFullName;
    issueNumber?: IssueNumber;
  } = {
    name: input.name,
    args: input.args,
  };
  if (input.repo !== undefined) {
    payload.repo = input.repo;
  }
  if (input.issueNumber !== undefined) {
    payload.issueNumber = input.issueNumber;
  }
  return payload;
}
