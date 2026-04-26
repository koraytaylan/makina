/**
 * config/load.ts — file IO and validation for the user's `config.json`.
 *
 * Wave 1's `src/config/schema.ts` froze the typed shape; this module is
 * the only place Wave 2+ reads the file from disk. The loader:
 *
 * 1. Expands a single leading `~/` to the user's home directory.
 * 2. Reads the file (UTF-8, JSON-with-comments tolerant via
 *    {@link "@std/jsonc"}).
 * 3. Pipes the parsed value through {@link parseConfig} so the caller
 *    receives the typed {@link Config}.
 *
 * Errors are surfaced as a {@link ConfigLoadError} whose `message`
 * embeds the failing field path (`github.appId`, `lifecycle.mergeMode`,
 * …) so the daemon and the TUI can render them verbatim. The error
 * exposes structured `issues` for callers that want to render their own
 * formatting.
 *
 * The loader intentionally does **not** expand `~/` recursively or
 * touch other paths inside the config (e.g. `github.privateKeyPath`,
 * `daemon.socketPath`, `workspace`). Each consumer expands the paths
 * relevant to it; the convention keeps config text human-readable.
 *
 * @module
 */

import * as jsonc from "@std/jsonc";

import { type Config, type ConfigValidationIssue, parseConfig } from "./schema.ts";

/**
 * Reason a {@link loadConfig} call failed.
 *
 * - `not-found`: the file at the resolved path does not exist.
 * - `read-failed`: the file exists but could not be read (permissions,
 *   IO error, …).
 * - `invalid-json`: the file exists but is not valid JSONC.
 * - `invalid-schema`: parsed JSON did not satisfy the
 *   {@link Config} schema; `issues` carries the per-field problems.
 */
export type ConfigLoadFailureKind =
  | "not-found"
  | "read-failed"
  | "invalid-json"
  | "invalid-schema";

/**
 * Error thrown by {@link loadConfig} when the file cannot be loaded.
 *
 * `message` always begins with the resolved path, then a one-line
 * summary, and (for `invalid-schema`) lists every failing field as
 * `<path>: <message>`. Catch sites can read `kind` to branch.
 *
 * @example
 * ```ts
 * try {
 *   const config = await loadConfig("~/.config/makina/config.json");
 * } catch (error) {
 *   if (error instanceof ConfigLoadError && error.kind === "not-found") {
 *     console.error("No config; run `makina setup` first.");
 *     Deno.exit(1);
 *   }
 *   throw error;
 * }
 * ```
 */
export class ConfigLoadError extends Error {
  /** Discriminator for the failure mode. */
  readonly kind: ConfigLoadFailureKind;
  /** Filesystem path the loader attempted to read (post-`~/` expansion). */
  readonly resolvedPath: string;
  /** Per-field validation problems, when `kind === "invalid-schema"`. */
  readonly issues: readonly ConfigValidationIssue[];

  /**
   * Construct a config-load error.
   *
   * @param kind Failure mode discriminator.
   * @param resolvedPath The path the loader actually tried to read.
   * @param message Human-readable summary; rendered verbatim by the
   *   TUI / daemon.
   * @param issues Field-level validation problems
   *   (empty unless `kind === "invalid-schema"`).
   */
  constructor(
    kind: ConfigLoadFailureKind,
    resolvedPath: string,
    message: string,
    issues: readonly ConfigValidationIssue[] = [],
  ) {
    super(message);
    this.name = "ConfigLoadError";
    this.kind = kind;
    this.resolvedPath = resolvedPath;
    this.issues = issues;
  }
}

/**
 * Expand a single leading `~/` into the user's home directory.
 *
 * Mirrors POSIX shell behavior: `~` (alone) and `~/` resolve to
 * `$HOME`. Anything else (`./foo`, `/abs`, `~user/...`) is returned
 * unchanged — Wave 1 does not promise to expand `~user`.
 *
 * Throws when the input begins with `~/` but no home directory can be
 * resolved (the loader callers all have a home, so this manifests only
 * in misconfigured environments).
 *
 * @param path Candidate path, possibly beginning with `~/`.
 * @returns The expanded path.
 * @throws Error when `~/` cannot be expanded because no home directory is
 *   configured.
 *
 * @example
 * ```ts
 * expandHome("~/.config/makina/config.json"); // → "/Users/me/.config/makina/config.json"
 * expandHome("/etc/makina/config.json");      // → "/etc/makina/config.json"
 * ```
 */
export function expandHome(path: string): string {
  if (path === "~") {
    return resolveHome();
  }
  if (path.startsWith("~/")) {
    return joinHome(path.slice(2));
  }
  return path;
}

function resolveHome(): string {
  // ADR-008 defers Windows: only POSIX `$HOME` is supported. WSL2 users
  // have a `$HOME` like any other Linux env, so this still covers them.
  const home = Deno.env.get("HOME");
  if (home === undefined || home.length === 0) {
    throw new Error(
      "cannot expand ~/: $HOME is not set",
    );
  }
  return home;
}

function joinHome(rest: string): string {
  const home = resolveHome();
  // The home dir typically lacks a trailing slash. Strip a leading slash
  // from `rest` before joining so we never produce `//`.
  const trimmedRest = rest.startsWith("/") ? rest.slice(1) : rest;
  return home.endsWith("/") ? `${home}${trimmedRest}` : `${home}/${trimmedRest}`;
}

/**
 * Read, parse, and validate the user's `config.json`.
 *
 * The loader reads the file at `path` (after expanding a single leading
 * `~/`), parses it as JSONC (so `// line comments` and trailing commas
 * are tolerated), and passes the resulting value through
 * {@link parseConfig}. On failure it raises a {@link ConfigLoadError}
 * whose `kind` lets callers branch (missing-file vs. malformed-JSON vs.
 * schema-invalid).
 *
 * @param path The config file path. May begin with `~/`.
 * @returns The validated, typed {@link Config}.
 * @throws {ConfigLoadError} for any failure to load.
 *
 * @example
 * ```ts
 * const config = await loadConfig("~/.config/makina/config.json");
 * console.log(config.github.defaultRepo);
 * ```
 */
export async function loadConfig(path: string): Promise<Config> {
  const resolvedPath = expandHome(path);

  let text: string;
  try {
    text = await Deno.readTextFile(resolvedPath);
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      throw new ConfigLoadError(
        "not-found",
        resolvedPath,
        `config file not found: ${resolvedPath}`,
      );
    }
    const reason = error instanceof Error ? error.message : String(error);
    throw new ConfigLoadError(
      "read-failed",
      resolvedPath,
      `failed to read config file ${resolvedPath}: ${reason}`,
    );
  }

  let parsed: unknown;
  try {
    parsed = jsonc.parse(text);
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    throw new ConfigLoadError(
      "invalid-json",
      resolvedPath,
      `config file ${resolvedPath} is not valid JSON/JSONC: ${reason}`,
    );
  }

  const result = parseConfig(parsed);
  if (!result.success) {
    const summary = result.issues
      .map((issue) => `  - ${formatIssuePath(issue.path)}: ${issue.message}`)
      .join("\n");
    throw new ConfigLoadError(
      "invalid-schema",
      resolvedPath,
      `config file ${resolvedPath} failed validation:\n${summary}`,
      result.issues,
    );
  }

  return result.data;
}

/**
 * Render a zod path as a dotted string, falling back to bracketed
 * notation for numeric indices.
 *
 * @param path The zod issue path.
 * @returns A human-readable rendering, e.g. `github.installations["a/b"]`.
 */
function formatIssuePath(path: readonly (string | number)[]): string {
  if (path.length === 0) {
    return "<root>";
  }
  let rendered = "";
  for (const segment of path) {
    if (typeof segment === "number") {
      rendered += `[${segment}]`;
    } else if (rendered.length === 0) {
      rendered = segment;
    } else if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(segment)) {
      rendered += `.${segment}`;
    } else {
      rendered += `[${JSON.stringify(segment)}]`;
    }
  }
  return rendered;
}
