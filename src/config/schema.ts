/**
 * config/schema.ts — zod schema for `config.json`.
 *
 * The schema is the single source of truth for the user-facing
 * configuration shape. Wave 2's loader (`src/config/load.ts`) reads the
 * file, parses JSON-with-comments, and pipes the result through
 * {@link parseConfig}; the loader is also responsible for `~/` path
 * expansion (zod intentionally does not perform IO). The schema rejects
 * out-of-range values with helpful zod paths so users see, for example,
 * `["lifecycle", "settlingWindowMilliseconds"]` rather than a generic
 * "validation failed".
 *
 * **Type contract.** The exported {@link Config} interface is the canonical
 * shape every consumer reads. The internal zod schema is annotated with
 * `z.ZodType<Config>` so any drift between the schema and the interface is
 * a compile error — the two declarations cannot diverge silently. The zod
 * schema itself is **not** part of the public surface (consumers do not
 * import zod types into their graph); they call {@link parseConfig}
 * instead.
 *
 * Defaults match {@link "../constants.ts"} so the loader can rely on the
 * schema to fill in optional fields.
 *
 * @module
 */

import { z } from "zod";

import {
  MAX_MAX_TASK_ITERATIONS,
  MAX_POLL_INTERVAL_MILLISECONDS,
  MAX_SETTLING_WINDOW_MILLISECONDS,
  MAX_TASK_ITERATIONS,
  MIN_GITHUB_APP_ID,
  MIN_GITHUB_INSTALLATION_ID,
  MIN_MAX_TASK_ITERATIONS,
  MIN_POLL_INTERVAL_MILLISECONDS,
  MIN_SETTLING_WINDOW_MILLISECONDS,
  POLL_INTERVAL_MILLISECONDS,
  SETTLING_WINDOW_MILLISECONDS,
} from "../constants.ts";

// ---------------------------------------------------------------------------
// Public type contracts
// ---------------------------------------------------------------------------

/**
 * Configuration for the GitHub App connection.
 */
export interface GitHubConfig {
  /**
   * GitHub App id (the integer printed on the App settings page, **not**
   * the client id).
   */
  readonly appId: number;
  /**
   * Filesystem path to the App's downloaded private key (PEM). May begin
   * with `~/`; expansion happens at the consumer boundary (the GitHub
   * App client expands before opening the file). The loader returns
   * this string verbatim — see `src/config/load.ts` for the
   * path-expansion contract.
   */
  readonly privateKeyPath: string;
  /**
   * Map of `<owner/repo>` to the installation id that grants the App
   * access. Built up by `makina setup`.
   */
  readonly installations: Readonly<Record<string, number>>;
  /**
   * Default repository for slash commands that omit `[owner/repo]`. Must
   * appear as a key in {@link GitHubConfig.installations}.
   */
  readonly defaultRepo: string;
}

/**
 * Configuration for the agent runner.
 */
export interface AgentConfig {
  /** Anthropic model id used for every agent run unless overridden. */
  readonly model: string;
  /** Permission mode forwarded to the Claude Agent SDK. */
  readonly permissionMode: "acceptEdits";
  /**
   * Maximum agent iterations a single task may consume before the
   * supervisor escalates it to `NEEDS_HUMAN`.
   */
  readonly maxIterationsPerTask: number;
}

/**
 * Configuration for the task lifecycle, settling window, and merge
 * behavior.
 */
export interface LifecycleConfig {
  /** Merge strategy applied once a task reaches `READY_TO_MERGE`. */
  readonly mergeMode: "squash" | "rebase" | "manual";
  /**
   * The supervisor must observe at least this many milliseconds of "no
   * stabilize work to do" before transitioning a task to
   * `READY_TO_MERGE`.
   */
  readonly settlingWindowMilliseconds: number;
  /** Per-task GitHub poll cadence, in milliseconds. */
  readonly pollIntervalMilliseconds: number;
  /**
   * If `true`, the worktree is preserved on `MERGED` (handy for manual
   * follow-up); otherwise it is removed.
   */
  readonly preserveWorktreeOnMerge: boolean;
}

/**
 * Configuration for the daemon process.
 */
export interface DaemonConfig {
  /**
   * Filesystem path of the Unix-domain socket. May begin with `~/`;
   * expansion happens at the consumer boundary (the daemon expands
   * before binding). The loader returns this string verbatim — see
   * `src/config/load.ts` for the path-expansion contract.
   */
  readonly socketPath: string;
  /**
   * If `true`, the TUI auto-spawns the daemon when no socket is
   * listening; otherwise, it reports the missing daemon to the user.
   */
  readonly autoStart: boolean;
}

/**
 * Configuration for TUI keybindings. Values follow the
 * `<modifier>+<key>` shape used by the Ink input handlers
 * (`ctrl+p`, `shift+tab`).
 */
export interface KeybindingsConfig {
  /** Key chord that toggles the command palette overlay. */
  readonly commandPalette: string;
  /** Key chord that toggles the task switcher overlay. */
  readonly taskSwitcher: string;
}

/**
 * Configuration for the TUI.
 */
export interface TuiConfig {
  /** Per-overlay keybinding chords. */
  readonly keybindings: KeybindingsConfig;
}

/**
 * Top-level shape of `config.json`.
 */
export interface Config {
  /** GitHub App connection. */
  readonly github: GitHubConfig;
  /** Agent runner. */
  readonly agent: AgentConfig;
  /** Task lifecycle and merge behavior. */
  readonly lifecycle: LifecycleConfig;
  /**
   * Filesystem path under which makina creates per-repo bare clones and
   * per-task worktrees. May begin with `~/`; expansion happens at the
   * consumer boundary (the worktree manager expands before creating
   * directories). The loader returns this string verbatim — see
   * `src/config/load.ts` for the path-expansion contract.
   */
  readonly workspace: string;
  /** Daemon process. */
  readonly daemon: DaemonConfig;
  /** TUI behavior. */
  readonly tui: TuiConfig;
}

/**
 * One field-level validation problem reported by {@link parseConfig}.
 *
 * `path` points at the offending field so callers can render messages
 * like `lifecycle.settlingWindowMilliseconds: ...`. The `code` matches
 * zod's `ZodIssueCode` strings (`too_small`, `invalid_type`,
 * `custom`, ...) without forcing zod's class types into the public
 * surface.
 */
export interface ConfigValidationIssue {
  /** Field path inside the config object. */
  readonly path: readonly (string | number)[];
  /** Human-readable description. */
  readonly message: string;
  /** Zod issue code (`invalid_type`, `too_small`, `custom`, ...). */
  readonly code: string;
}

/**
 * Result of {@link parseConfig}. Either a successfully-parsed
 * {@link Config} or a non-empty list of {@link ConfigValidationIssue}
 * values describing every failing field.
 */
export type ParseConfigResult =
  | { readonly success: true; readonly data: Config }
  | { readonly success: false; readonly issues: readonly ConfigValidationIssue[] };

// ---------------------------------------------------------------------------
// Internal zod schemas (not exported — call `parseConfig` instead)
// ---------------------------------------------------------------------------

const repoFullNameSchema = z
  .string()
  .trim()
  .regex(/^[^\s/]+\/[^\s/]+$/, {
    message: 'Must be "<owner>/<name>"',
  });

const installationIdSchema = z
  .number()
  .int()
  .gte(MIN_GITHUB_INSTALLATION_ID);

const githubConfigSchema: z.ZodType<GitHubConfig, z.ZodTypeDef, unknown> = z.object({
  appId: z.number().int().gte(MIN_GITHUB_APP_ID),
  privateKeyPath: z.string().min(1),
  installations: z.record(repoFullNameSchema, installationIdSchema),
  defaultRepo: repoFullNameSchema,
});

const agentConfigSchema: z.ZodType<AgentConfig, z.ZodTypeDef, unknown> = z.object({
  model: z.string().min(1),
  permissionMode: z.enum(["acceptEdits"]),
  maxIterationsPerTask: z
    .number()
    .int()
    .gte(MIN_MAX_TASK_ITERATIONS)
    .lte(MAX_MAX_TASK_ITERATIONS)
    .default(MAX_TASK_ITERATIONS),
});

const lifecycleConfigSchema: z.ZodType<LifecycleConfig, z.ZodTypeDef, unknown> = z.object({
  mergeMode: z.enum(["squash", "rebase", "manual"]),
  settlingWindowMilliseconds: z
    .number()
    .int()
    .gte(MIN_SETTLING_WINDOW_MILLISECONDS)
    .lte(MAX_SETTLING_WINDOW_MILLISECONDS)
    .default(SETTLING_WINDOW_MILLISECONDS),
  pollIntervalMilliseconds: z
    .number()
    .int()
    .gte(MIN_POLL_INTERVAL_MILLISECONDS)
    .lte(MAX_POLL_INTERVAL_MILLISECONDS)
    .default(POLL_INTERVAL_MILLISECONDS),
  preserveWorktreeOnMerge: z.boolean().default(false),
});

const daemonConfigSchema: z.ZodType<DaemonConfig, z.ZodTypeDef, unknown> = z.object({
  socketPath: z.string().min(1),
  autoStart: z.boolean().default(true),
});

const keybindingsConfigSchema: z.ZodType<KeybindingsConfig, z.ZodTypeDef, unknown> = z
  .object({
    commandPalette: z.string().min(1).default("ctrl+p"),
    taskSwitcher: z.string().min(1).default("ctrl+g"),
  });

const tuiConfigSchema: z.ZodType<TuiConfig, z.ZodTypeDef, unknown> = z.object({
  keybindings: keybindingsConfigSchema.default({
    commandPalette: "ctrl+p",
    taskSwitcher: "ctrl+g",
  }),
});

const configSchema: z.ZodType<Config, z.ZodTypeDef, unknown> = z
  .object({
    github: githubConfigSchema,
    agent: agentConfigSchema,
    lifecycle: lifecycleConfigSchema,
    workspace: z.string().min(1),
    daemon: daemonConfigSchema,
    tui: tuiConfigSchema.default({
      keybindings: { commandPalette: "ctrl+p", taskSwitcher: "ctrl+g" },
    }),
  })
  .superRefine((value, context) => {
    const installations = value.github.installations;
    const defaultRepo = value.github.defaultRepo;
    if (!Object.prototype.hasOwnProperty.call(installations, defaultRepo)) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        path: ["github", "defaultRepo"],
        message: `defaultRepo "${defaultRepo}" is not present in github.installations`,
      });
    }
  });

// ---------------------------------------------------------------------------
// Public parse API
// ---------------------------------------------------------------------------

/**
 * Parse an arbitrary JSON value as a {@link Config}. Returns either
 * `{ success: true, data }` or `{ success: false, error }` so callers can
 * handle validation failures without `try`/`catch`.
 *
 * The cross-field invariant
 * `github.defaultRepo ∈ keys(github.installations)` is enforced; the
 * resulting error path is `["github", "defaultRepo"]`.
 *
 * @param raw The candidate value (typically the result of `JSON.parse`).
 * @returns The parsed config or the zod error.
 *
 * @example
 * ```ts
 * import { parseConfig } from "./schema.ts";
 *
 * const text = await Deno.readTextFile(path);
 * const result = parseConfig(JSON.parse(text));
 * if (!result.success) {
 *   for (const issue of result.issues) {
 *     console.error(issue.path.join("."), issue.message);
 *   }
 *   Deno.exit(1);
 * }
 * const config: Config = result.data;
 * ```
 */
export function parseConfig(raw: unknown): ParseConfigResult {
  const result = configSchema.safeParse(raw);
  if (result.success) {
    return { success: true, data: result.data };
  }
  const issues: ConfigValidationIssue[] = result.error.issues.map((issue) => ({
    path: [...issue.path],
    message: issue.message,
    code: issue.code,
  }));
  return { success: false, issues };
}
