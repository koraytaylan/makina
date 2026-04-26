/**
 * config/setup-wizard.ts — interactive `makina setup` wizard.
 *
 * Walks a first-time user through the four pieces of state that cannot
 * be defaulted: the GitHub App id, the path to its private key, the
 * installation ids granting access to each repository, and which
 * repository is the default. The remaining config is filled with the
 * documented defaults from {@link "../constants.ts"}.
 *
 * The wizard's collaborators (stdin reader, stdout writer, and the
 * narrow GitHub client used for installation discovery) are
 * **injected** so the integration test in
 * `tests/integration/setup_wizard_test.ts` can drive a scripted
 * conversation against the in-memory GitHub client without touching a
 * real terminal or real network. `runSetupWizard` is therefore a pure
 * orchestrator over `io`; the production `setup` subcommand assembles
 * an `io` from `Deno.stdin`, `Deno.stdout`, and the real GitHub App
 * client.
 *
 * Side-effects the wizard intentionally avoids:
 *   - It does **not** write the resulting config to disk; the caller
 *     decides where (the `setup` subcommand in `main.ts` writes to the
 *     platform-appropriate path).
 *   - It does **not** authenticate to GitHub. App-auth is `[W2-github-app-auth]`;
 *     the wizard only invokes the App-level installation listing, which
 *     `src/github/app-auth.ts` will eventually expose to it.
 *
 * @module
 */

import { exists } from "@std/fs";

import { type Config, type GitHubConfig, parseConfig } from "./schema.ts";
import { expandHome } from "./load.ts";
import {
  MAX_TASK_ITERATIONS,
  POLL_INTERVAL_MILLISECONDS,
  SETTLING_WINDOW_MILLISECONDS,
} from "../constants.ts";

/**
 * One installation reachable from the GitHub App.
 *
 * Returned by {@link WizardGitHubClient.getInstallations}. The fields
 * mirror what the wizard needs to render a numbered picker: the
 * `installationId` is the integer the API uses to mint installation
 * tokens, and `repositories` is the set of `<owner/repo>` slugs the
 * installation grants access to.
 */
export interface WizardInstallation {
  /** The numeric installation id used by `POST /app/installations/{id}/access_tokens`. */
  readonly installationId: number;
  /** The repositories this installation can act on, as `<owner>/<name>` strings. */
  readonly repositories: readonly string[];
}

/**
 * Narrow, wizard-only GitHub client surface.
 *
 * The full {@link "../types.ts".GitHubClient} contract is per-installation
 * (it expects an installation token); the wizard runs **before** any
 * installation is selected, so it needs an App-level call to discover
 * installations. We declare a separate one-method surface here rather
 * than widen the consumer-facing client.
 *
 * Wave 2's `src/github/app-auth.ts` (issue `[W2-github-app-auth]`)
 * provides the production implementation against the GitHub App's
 * `GET /app/installations` endpoint. The wizard's tests stub this
 * directly.
 */
export interface WizardGitHubClient {
  /**
   * List every installation reachable from the configured App.
   *
   * @param args App credentials needed to mint the App JWT.
   * @returns Every reachable installation along with the repositories it
   *   grants access to. Returns an empty array if the App is not
   *   installed anywhere.
   */
  getInstallations(args: {
    readonly appId: number;
    readonly privateKeyPath: string;
  }): Promise<readonly WizardInstallation[]>;
}

/**
 * Read one logical line of input.
 *
 * Returns the line **without** the trailing newline. A return of
 * `null` is treated as EOF and aborts the wizard with a helpful
 * message; production code wires this to a `TextLineStream`-backed
 * reader.
 */
export type ReadLine = () => Promise<string | null>;

/** Write one chunk of output (a prompt or a status line). */
export type WriteLine = (text: string) => Promise<void> | void;

/**
 * Injected collaborators for {@link runSetupWizard}.
 *
 * Tests pass scripted readers and a mock GitHub client; production
 * code wires `Deno.stdin`, `Deno.stdout`, and the real App client.
 */
export interface SetupWizardIo {
  /** Read one line from the user (no trailing newline). */
  readonly readLine: ReadLine;
  /** Write one chunk of output (the wizard adds its own newline). */
  readonly writeLine: WriteLine;
  /** GitHub client for App-level installation discovery. */
  readonly githubClient: WizardGitHubClient;
}

/**
 * Error raised when the wizard cannot complete.
 *
 * Distinguished from generic `Error` so the `setup` subcommand can
 * print a tidy single-line summary instead of a stack trace.
 */
export class SetupWizardError extends Error {
  /**
   * Construct a wizard error.
   *
   * @param message Human-readable description of the failure.
   */
  constructor(message: string) {
    super(message);
    this.name = "SetupWizardError";
  }
}

/**
 * Run the interactive `setup` wizard against the supplied IO.
 *
 * Sequence:
 *   1. Prompt for the App ID (positive integer).
 *   2. Prompt for the private-key path (must exist on disk; `~/`
 *      expansion happens here so the user can paste the literal path
 *      they kept the file at).
 *   3. Call `githubClient.getInstallations(...)`.
 *   4. Render every accessible repository with a number; prompt for the
 *      default repo selection.
 *   5. Build a {@link Config} from the answers + defaults and revalidate
 *      it through {@link parseConfig} so the returned object is
 *      indistinguishable from one loaded from disk.
 *
 * @param io Injected collaborators. Tests pass scripted versions; the
 *   `setup` subcommand wires real stdin/stdout and the App client.
 * @returns The validated {@link Config}. The caller is responsible for
 *   writing it to the platform-appropriate path.
 * @throws {SetupWizardError} on user error (bad input, missing key, no
 *   installations, EOF on stdin).
 *
 * @example
 * ```ts
 * const config = await runSetupWizard({
 *   readLine: scriptedLines(["1234567", "/tmp/key.pem", "1"]),
 *   writeLine: (text) => { capturedOutput.push(text); },
 *   githubClient: stubClient,
 * });
 * await Deno.writeTextFile(configPath, JSON.stringify(config, null, 2));
 * ```
 */
export async function runSetupWizard(io: SetupWizardIo): Promise<Config> {
  await io.writeLine("makina setup — first-run configuration wizard");
  await io.writeLine("");
  await io.writeLine(
    "This walks through the GitHub App connection. See docs/setup-github-app.md",
  );
  await io.writeLine("for details on creating the App itself.");
  await io.writeLine("");

  const appId = await promptAppId(io);
  const privateKeyPath = await promptPrivateKeyPath(io);
  const installations = await fetchInstallations(io, appId, privateKeyPath);
  const repoChoices = collectRepositoryChoices(installations);
  if (repoChoices.length === 0) {
    throw new SetupWizardError(
      "no repositories reachable: install the App on at least one repo (see docs/setup-github-app.md) and rerun `makina setup`.",
    );
  }
  const defaultRepo = await promptDefaultRepo(io, repoChoices);

  const installationsRecord = repoChoicesToRecord(repoChoices);
  const candidate = buildConfig({
    appId,
    privateKeyPath,
    installations: installationsRecord,
    defaultRepo,
  });

  const parsed = parseConfig(candidate);
  if (!parsed.success) {
    const summary = parsed.issues
      .map((issue) => `  - ${issue.path.join(".")}: ${issue.message}`)
      .join("\n");
    throw new SetupWizardError(
      `internal error: wizard produced an invalid config:\n${summary}`,
    );
  }

  await io.writeLine("");
  await io.writeLine(
    `Done. Default repo: ${defaultRepo}; ${repoChoices.length} repository(ies) wired up.`,
  );
  return parsed.data;
}

/**
 * Minimal contract for an object that reads UTF-8 bytes from a source.
 *
 * Mirrors the shape of `Deno.stdin.read`. The wizard only needs the
 * bytes-in interface so tests can swap in a buffer-backed reader.
 */
export interface ByteReader {
  /**
   * Read up to `buffer.length` bytes into `buffer`. Returns the number
   * of bytes read, or `null` at end-of-stream.
   *
   * @param buffer Destination buffer.
   * @returns Bytes read, or `null` at EOF.
   */
  read(buffer: Uint8Array): Promise<number | null>;
}

/**
 * Minimal contract for an object that writes UTF-8 bytes to a sink.
 *
 * Mirrors the shape of `Deno.stdout.write`. The wizard only needs the
 * bytes-out interface so tests can swap in a buffer-backed writer.
 */
export interface ByteWriter {
  /**
   * Write the entire buffer. Returns the number of bytes written.
   *
   * @param buffer Bytes to write.
   * @returns Bytes written.
   */
  write(buffer: Uint8Array): Promise<number>;
}

/**
 * Build a {@link SetupWizardIo} backed by an arbitrary byte reader and
 * writer. The reader is decoded as UTF-8 and split on `\n` (a trailing
 * `\r` is stripped so Windows-style stdin Just Works). The writer
 * receives `${text}\n` per call.
 *
 * Tests use this with `Deno.Buffer`-backed reader/writer pairs;
 * production code goes through {@link createStdioWizardIo}.
 *
 * @param reader Byte source (typically `Deno.stdin`).
 * @param writer Byte sink (typically `Deno.stdout`).
 * @param githubClient Wizard-only GitHub client.
 * @returns An IO bundle suitable for {@link runSetupWizard}.
 */
export function createWizardIo(
  reader: ByteReader,
  writer: ByteWriter,
  githubClient: WizardGitHubClient,
): SetupWizardIo {
  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  let buffer = "";
  let eof = false;
  const readChunkBytes = 1_024;

  const readLine: ReadLine = async () => {
    while (true) {
      const newlineIndex = buffer.indexOf("\n");
      if (newlineIndex !== -1) {
        const line = buffer.slice(0, newlineIndex);
        buffer = buffer.slice(newlineIndex + 1);
        // Strip a trailing CR so Windows-style stdin behaves like *nix.
        return line.endsWith("\r") ? line.slice(0, -1) : line;
      }
      if (eof) {
        if (buffer.length === 0) {
          return null;
        }
        const remainder = buffer;
        buffer = "";
        return remainder.endsWith("\r") ? remainder.slice(0, -1) : remainder;
      }
      const chunk = new Uint8Array(readChunkBytes);
      const bytesRead = await reader.read(chunk);
      if (bytesRead === null) {
        eof = true;
        continue;
      }
      buffer += decoder.decode(chunk.subarray(0, bytesRead), { stream: true });
    }
  };

  const writeLine: WriteLine = async (text: string) => {
    await writer.write(encoder.encode(`${text}\n`));
  };

  return { readLine, writeLine, githubClient };
}

/**
 * Build the production-time {@link SetupWizardIo} backed by `Deno.stdin`,
 * `Deno.stdout`, and the supplied GitHub client.
 *
 * Thin convenience wrapper around {@link createWizardIo}; the `setup`
 * subcommand calls this and tests cover the underlying logic by
 * passing in-memory readers/writers to {@link createWizardIo} directly.
 *
 * @param githubClient The wizard-only GitHub client (typically backed by
 *   `src/github/app-auth.ts`).
 * @returns A live IO bundle suitable for {@link runSetupWizard}.
 */
export function createStdioWizardIo(
  githubClient: WizardGitHubClient,
): SetupWizardIo {
  return createWizardIo(Deno.stdin, Deno.stdout, githubClient);
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

async function promptAppId(io: SetupWizardIo): Promise<number> {
  await io.writeLine("GitHub App ID (integer printed at the top of the App settings page):");
  const raw = await readRequiredLine(io, "App ID");
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isInteger(parsed) || parsed < 1 || `${parsed}` !== raw) {
    throw new SetupWizardError(
      `invalid App ID ${JSON.stringify(raw)}: expected a positive integer`,
    );
  }
  return parsed;
}

async function promptPrivateKeyPath(io: SetupWizardIo): Promise<string> {
  await io.writeLine("");
  await io.writeLine("Path to the App's private key (PEM). May begin with ~/.");
  const raw = await readRequiredLine(io, "Private-key path");
  const expanded = expandHome(raw);
  const found = await exists(expanded, { isFile: true });
  if (!found) {
    throw new SetupWizardError(
      `private key not found at ${expanded}. Re-run after saving the .pem there.`,
    );
  }
  // Persist the user-provided form (still possibly containing ~/) so the
  // file stays portable across machines.
  return raw;
}

async function fetchInstallations(
  io: SetupWizardIo,
  appId: number,
  privateKeyPath: string,
): Promise<readonly WizardInstallation[]> {
  await io.writeLine("");
  await io.writeLine("Discovering installations...");
  let installations: readonly WizardInstallation[];
  try {
    installations = await io.githubClient.getInstallations({
      appId,
      privateKeyPath,
    });
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    throw new SetupWizardError(
      `failed to list installations: ${reason}`,
    );
  }
  if (installations.length === 0) {
    throw new SetupWizardError(
      "the App is not installed anywhere. Install it on at least one repo (see docs/setup-github-app.md) and rerun `makina setup`.",
    );
  }
  return installations;
}

interface RepoChoice {
  readonly repo: string;
  readonly installationId: number;
}

function collectRepositoryChoices(
  installations: readonly WizardInstallation[],
): readonly RepoChoice[] {
  // Flatten installation × repository into a single picker. If the same
  // repo appears under multiple installations the first one wins; the
  // wizard logs a note rather than failing because that mostly happens
  // when a personal install and an org install overlap.
  const seen = new Set<string>();
  const choices: RepoChoice[] = [];
  for (const installation of installations) {
    for (const repo of installation.repositories) {
      if (seen.has(repo)) {
        continue;
      }
      seen.add(repo);
      choices.push({ repo, installationId: installation.installationId });
    }
  }
  return choices;
}

function repoChoicesToRecord(
  choices: readonly RepoChoice[],
): Record<string, number> {
  const record: Record<string, number> = {};
  for (const choice of choices) {
    record[choice.repo] = choice.installationId;
  }
  return record;
}

async function promptDefaultRepo(
  io: SetupWizardIo,
  choices: readonly RepoChoice[],
): Promise<string> {
  await io.writeLine("");
  await io.writeLine("Reachable repositories:");
  choices.forEach((choice, index) => {
    // Wizard picker numbers are one-based — easier to type at a prompt.
    const label = `  ${index + 1}. ${choice.repo}`;
    void io.writeLine(label);
  });
  await io.writeLine("");
  await io.writeLine(
    `Pick the default repository (1-${choices.length}). Slash commands without an explicit [owner/repo] target this one.`,
  );
  const raw = await readRequiredLine(io, "Default repo");
  const numeric = Number.parseInt(raw, 10);
  if (!Number.isInteger(numeric) || `${numeric}` !== raw) {
    throw new SetupWizardError(
      `invalid choice ${JSON.stringify(raw)}: expected a number between 1 and ${choices.length}.`,
    );
  }
  if (numeric < 1 || numeric > choices.length) {
    throw new SetupWizardError(
      `choice ${numeric} is out of range; pick between 1 and ${choices.length}.`,
    );
  }
  // numeric is 1..choices.length so the lookup is safe; the helper exists
  // to satisfy noUncheckedIndexedAccess.
  const chosen = choices[numeric - 1];
  if (chosen === undefined) {
    throw new SetupWizardError("internal error: out-of-range repository choice");
  }
  return chosen.repo;
}

async function readRequiredLine(io: SetupWizardIo, label: string): Promise<string> {
  await io.writeLine("> ");
  const line = await io.readLine();
  if (line === null) {
    throw new SetupWizardError(
      `unexpected end of input while reading ${label}; aborting.`,
    );
  }
  const trimmed = line.trim();
  if (trimmed.length === 0) {
    throw new SetupWizardError(`${label} cannot be empty.`);
  }
  return trimmed;
}

interface ConfigSeeds {
  readonly appId: number;
  readonly privateKeyPath: string;
  readonly installations: Record<string, number>;
  readonly defaultRepo: string;
}

function buildConfig(seeds: ConfigSeeds): unknown {
  const github: GitHubConfig = {
    appId: seeds.appId,
    privateKeyPath: seeds.privateKeyPath,
    installations: seeds.installations,
    defaultRepo: seeds.defaultRepo,
  };
  return {
    github,
    agent: {
      model: "claude-sonnet-4-6",
      permissionMode: "acceptEdits",
      maxIterationsPerTask: MAX_TASK_ITERATIONS,
    },
    lifecycle: {
      mergeMode: "squash",
      settlingWindowMilliseconds: SETTLING_WINDOW_MILLISECONDS,
      pollIntervalMilliseconds: POLL_INTERVAL_MILLISECONDS,
      preserveWorktreeOnMerge: false,
    },
    workspace: defaultWorkspacePath(),
    daemon: {
      socketPath: defaultSocketPath(),
      autoStart: true,
    },
    tui: {
      keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" },
    },
  };
}

/**
 * Resolve the platform-appropriate default workspace path.
 *
 * Returned as a `~/`-prefixed string so `config.json` stays portable
 * across machines belonging to the same user.
 *
 * @returns The default workspace path expressed with a leading `~/`.
 */
function defaultWorkspacePath(): string {
  if (Deno.build.os === "darwin") {
    return "~/Library/Application Support/makina/workspace";
  }
  return "~/.local/share/makina/workspace";
}

/**
 * Resolve the platform-appropriate default daemon socket path.
 *
 * @returns The default socket path expressed with a leading `~/`.
 */
function defaultSocketPath(): string {
  if (Deno.build.os === "darwin") {
    return "~/Library/Application Support/makina/daemon.sock";
  }
  return "~/.local/share/makina/daemon.sock";
}

/**
 * Resolve the platform-appropriate path the `setup` subcommand should
 * write the produced {@link Config} to.
 *
 * macOS uses the Apple-defined Application Support directory; Linux
 * follows the XDG Base Directory specification (`~/.config/...`). The
 * path is `~/`-prefixed so callers can pass it straight to
 * {@link expandHome}.
 *
 * @returns The default config-file path with a leading `~/`.
 *
 * @example
 * ```ts
 * import { expandHome } from "./load.ts";
 * import { defaultConfigPath } from "./setup-wizard.ts";
 *
 * const path = expandHome(defaultConfigPath());
 * await Deno.writeTextFile(path, JSON.stringify(config, null, 2));
 * ```
 */
export function defaultConfigPath(): string {
  if (Deno.build.os === "darwin") {
    return "~/Library/Application Support/makina/config.json";
  }
  return "~/.config/makina/config.json";
}
