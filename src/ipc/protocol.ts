/**
 * ipc/protocol.ts — typed contracts for every IPC message that flows
 * between the TUI and the daemon over the Unix-domain socket.
 *
 * The shared envelope is `{ id, type, payload }`. `id` is a client-chosen
 * correlation identifier echoed back in `ack` and `pong` so the client
 * can resolve outstanding requests; `type` is the discriminator that
 * selects the payload shape below.
 *
 * **Type contract.** Each payload is declared as a TypeScript interface
 * (the public type the consumer reads) backed by an internal zod schema
 * (the runtime validator). The `z.ZodType<TheInterface, …>` annotation
 * makes any drift between the two a compile error so the public surface
 * and the runtime check cannot diverge silently. Consumers do **not**
 * import zod from this module — they call {@link parseEnvelope} (or the
 * codec, which wraps it).
 *
 * **Why zod-on-both-sides.** Wave 2 wires the daemon and the TUI on
 * opposite ends of the socket; both ends parse with the same validator
 * so a malformed or stale-versioned client can never poison the daemon
 * (or vice versa).
 *
 * **Direction.** Each payload's JSDoc names the direction it flows in
 * (`client→daemon` or `daemon→client`). The codec accepts either, and
 * the daemon and the TUI each apply a directional whitelist after
 * parsing.
 *
 * @module
 */

import { z } from "zod";

import {
  type IssueNumber,
  makeIssueNumber,
  makeRepoFullName,
  type RepoFullName,
  type StabilizePhase,
  type TaskState,
} from "../types.ts";

// ---------------------------------------------------------------------------
// Public type contracts
// ---------------------------------------------------------------------------

/**
 * Liveness check payload. Empty by design; the envelope's `id` carries
 * the correlation.
 *
 * Direction: client→daemon.
 */
export type PingPayload = Record<string, never>;

/**
 * Subscribe-to-events payload.
 *
 * Direction: client→daemon.
 */
export interface SubscribePayload {
  /**
   * The task id to subscribe to, or the wildcard `"*"` for every task.
   */
  readonly target: string;
}

/**
 * Unsubscribe payload.
 *
 * Direction: client→daemon.
 */
export interface UnsubscribePayload {
  /** The `id` of the original `subscribe` envelope. */
  readonly subscriptionId: string;
}

/**
 * Slash-command invocation payload. `name` is the command without the
 * leading `/`, e.g. `"issue"`, `"merge"`, `"daemon"`. `args` is the
 * post-name token stream as parsed by Wave 3's slash-command parser.
 *
 * Direction: client→daemon.
 */
export interface CommandPayload {
  /** Command name without the leading `/`. */
  readonly name: string;
  /** Positional argument tokens, in order. */
  readonly args: readonly string[];
  /** Optional `<owner>/<name>` repository scope override. */
  readonly repo?: RepoFullName | undefined;
  /** Optional issue number scope override. */
  readonly issueNumber?: IssueNumber | undefined;
}

/**
 * Free-form text the user is sending into a focused task's agent
 * session.
 *
 * Direction: client→daemon.
 */
export interface PromptPayload {
  /** Task whose session should receive the prompt. */
  readonly taskId: string;
  /** Prompt text (must be non-empty). */
  readonly text: string;
}

/**
 * Reply to a `ping`.
 *
 * Direction: daemon→client.
 */
export interface PongPayload {
  /** Daemon's reported version string. */
  readonly daemonVersion: string;
}

/**
 * Acknowledgement for any client→daemon request that does not have a
 * dedicated reply schema (`subscribe`, `unsubscribe`, `command`,
 * `prompt`).
 *
 * Direction: daemon→client.
 */
export interface AckPayload {
  /** Whether the request was accepted. */
  readonly ok: boolean;
  /** Failure description when {@link AckPayload.ok} is `false`. */
  readonly error?: string | undefined;
}

/** Per-kind data payload for a `state-changed` event. */
export interface StateChangedData {
  /** State before the transition. */
  readonly fromState: TaskState;
  /** State after the transition. */
  readonly toState: TaskState;
  /** Active stabilize phase after the transition, when relevant. */
  readonly stabilizePhase?: StabilizePhase | undefined;
  /** Optional one-line rationale for the transition. */
  readonly reason?: string | undefined;
}

/** Per-kind data payload for a `log` event. */
export interface LogData {
  /** Severity. */
  readonly level: "debug" | "info" | "warn" | "error";
  /** Free-form message. */
  readonly message: string;
}

/** Per-kind data payload for an `agent-message` event. */
export interface AgentMessageData {
  /** Direction the message flows. */
  readonly role: "user" | "assistant" | "system" | "tool-use" | "tool-result";
  /** Best-effort plain-text rendering of the message. */
  readonly text: string;
}

/** Per-kind data payload for a `github-call` event. */
export interface GitHubCallData {
  /** HTTP method. */
  readonly method: string;
  /** Path or operation name. */
  readonly endpoint: string;
  /** Status code, when known. */
  readonly statusCode?: number | undefined;
  /** Wall-clock duration of the request, in milliseconds. */
  readonly durationMilliseconds?: number | undefined;
  /**
   * Single-line error message when the call rejected (network error,
   * 4xx/5xx that the client surfaces, JSON parse failure, etc.).
   * Absent on successful calls. Mirrors
   * {@link "../types.ts".GitHubCallPayload.error} so the daemon can
   * publish a `github-call` event for a failing request and the IPC
   * codec round-trips it through the strict validator without
   * stripping or rejecting the field.
   */
  readonly error?: string | undefined;
}

/** Per-kind data payload for an `error` event. */
export interface ErrorData {
  /** Single-sentence description suitable for a status bar. */
  readonly message: string;
  /** Optional stack trace or extended diagnostic. */
  readonly detail?: string | undefined;
}

/**
 * Single event published from a task. The discriminated union mirrors
 * {@link "../types.ts".TaskEvent} (minus the `taskId` and `atIso`
 * headers, which the envelope already carries).
 *
 * Direction: daemon→client.
 */
export type EventPayload =
  & {
    /** Task this event belongs to. */
    readonly taskId: string;
    /** ISO-8601 timestamp at publish time. */
    readonly atIso: string;
  }
  & (
    | { readonly kind: "state-changed"; readonly data: StateChangedData }
    | { readonly kind: "log"; readonly data: LogData }
    | { readonly kind: "agent-message"; readonly data: AgentMessageData }
    | { readonly kind: "github-call"; readonly data: GitHubCallData }
    | { readonly kind: "error"; readonly data: ErrorData }
  );

/**
 * Tag values for {@link MessageEnvelope.type}. Adding a new message type
 * is a contract change; consumers exhaustive-switch on this union.
 */
export const MESSAGE_TYPES = [
  "ping",
  "subscribe",
  "unsubscribe",
  "command",
  "prompt",
  "pong",
  "ack",
  "event",
] as const;

/** Discriminant union of valid {@link MessageEnvelope.type} values. */
export type MessageType = typeof MESSAGE_TYPES[number];

/**
 * Discriminated envelope.
 *
 * Each per-type variant pairs the `type` literal with its payload type.
 * Directional whitelisting (the daemon refusing a `pong` from a client,
 * etc.) is the responsibility of the receiver, not the codec.
 *
 * @example
 * ```ts
 * const envelope: MessageEnvelope = {
 *   id: "1",
 *   type: "ping",
 *   payload: {},
 * };
 * ```
 */
export type MessageEnvelope =
  | { readonly id: string; readonly type: "ping"; readonly payload: PingPayload }
  | { readonly id: string; readonly type: "subscribe"; readonly payload: SubscribePayload }
  | { readonly id: string; readonly type: "unsubscribe"; readonly payload: UnsubscribePayload }
  | { readonly id: string; readonly type: "command"; readonly payload: CommandPayload }
  | { readonly id: string; readonly type: "prompt"; readonly payload: PromptPayload }
  | { readonly id: string; readonly type: "pong"; readonly payload: PongPayload }
  | { readonly id: string; readonly type: "ack"; readonly payload: AckPayload }
  | { readonly id: string; readonly type: "event"; readonly payload: EventPayload };

/**
 * One field-level validation problem reported by {@link parseEnvelope}.
 *
 * Mirrors the shape used by `src/config/schema.ts` so consumers learn
 * one error vocabulary, not two. `path` points at the offending field;
 * `code` is zod's `ZodIssueCode` value (`invalid_type`, `too_small`,
 * `custom`, ...).
 */
export interface EnvelopeValidationIssue {
  /** Field path inside the envelope. */
  readonly path: readonly (string | number)[];
  /** Human-readable description. */
  readonly message: string;
  /** Zod issue code. */
  readonly code: string;
}

/**
 * Result of {@link parseEnvelope}. Either a successfully-parsed
 * {@link MessageEnvelope} or a non-empty list of
 * {@link EnvelopeValidationIssue} values describing every failing field.
 */
export type ParseEnvelopeResult =
  | { readonly success: true; readonly data: MessageEnvelope }
  | { readonly success: false; readonly issues: readonly EnvelopeValidationIssue[] };

// ---------------------------------------------------------------------------
// Internal zod validators (not exported; consumers call `parseEnvelope`)
// ---------------------------------------------------------------------------

const messageIdSchema = z.string().min(1);

const repoFullNameSchema = z
  .string()
  .min(1)
  .transform((raw, context): RepoFullName => {
    try {
      return makeRepoFullName(raw);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      context.addIssue({ code: z.ZodIssueCode.custom, message });
      return z.NEVER;
    }
  });

const issueNumberSchema = z
  .number()
  .transform((raw, context): IssueNumber => {
    try {
      return makeIssueNumber(raw);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      context.addIssue({ code: z.ZodIssueCode.custom, message });
      return z.NEVER;
    }
  });

const pingPayloadSchema: z.ZodType<PingPayload, z.ZodTypeDef, unknown> = z.object({}).strict();

const subscribePayloadSchema: z.ZodType<SubscribePayload, z.ZodTypeDef, unknown> = z
  .object({
    target: z.union([z.literal("*"), z.string().min(1)]),
  })
  .strict();

const unsubscribePayloadSchema: z.ZodType<UnsubscribePayload, z.ZodTypeDef, unknown> = z
  .object({
    subscriptionId: messageIdSchema,
  })
  .strict();

const commandPayloadSchema: z.ZodType<CommandPayload, z.ZodTypeDef, unknown> = z
  .object({
    name: z.string().min(1),
    args: z.array(z.string()).default([]),
    repo: repoFullNameSchema.optional(),
    issueNumber: issueNumberSchema.optional(),
  })
  .strict();

const promptPayloadSchema: z.ZodType<PromptPayload, z.ZodTypeDef, unknown> = z
  .object({
    taskId: z.string().min(1),
    text: z.string().min(1),
  })
  .strict();

const pongPayloadSchema: z.ZodType<PongPayload, z.ZodTypeDef, unknown> = z
  .object({
    daemonVersion: z.string().min(1),
  })
  .strict();

const ackPayloadSchema: z.ZodType<AckPayload, z.ZodTypeDef, unknown> = z
  .object({
    ok: z.boolean(),
    error: z.string().optional(),
  })
  .strict();

const taskStateValues: readonly [TaskState, ...TaskState[]] = [
  "INIT",
  "CLONING_WORKTREE",
  "DRAFTING",
  "COMMITTING",
  "PUSHING",
  "PR_OPEN",
  "STABILIZING",
  "READY_TO_MERGE",
  "MERGED",
  "NEEDS_HUMAN",
  "FAILED",
];
const taskStateSchema = z.enum(taskStateValues);
const stabilizePhaseSchema = z.enum(["REBASE", "CI", "CONVERSATIONS"]);

const stateChangedDataSchema: z.ZodType<StateChangedData, z.ZodTypeDef, unknown> = z
  .object({
    fromState: taskStateSchema,
    toState: taskStateSchema,
    stabilizePhase: stabilizePhaseSchema.optional(),
    reason: z.string().optional(),
  })
  .strict();

const logDataSchema: z.ZodType<LogData, z.ZodTypeDef, unknown> = z
  .object({
    level: z.enum(["debug", "info", "warn", "error"]),
    message: z.string(),
  })
  .strict();

const agentMessageDataSchema: z.ZodType<AgentMessageData, z.ZodTypeDef, unknown> = z
  .object({
    role: z.enum(["user", "assistant", "system", "tool-use", "tool-result"]),
    text: z.string(),
  })
  .strict();

const githubCallDataSchema: z.ZodType<GitHubCallData, z.ZodTypeDef, unknown> = z
  .object({
    method: z.string().min(1),
    endpoint: z.string().min(1),
    statusCode: z.number().int().optional(),
    durationMilliseconds: z.number().nonnegative().optional(),
    error: z.string().optional(),
  })
  .strict();

const errorDataSchema: z.ZodType<ErrorData, z.ZodTypeDef, unknown> = z
  .object({
    message: z.string().min(1),
    detail: z.string().optional(),
  })
  .strict();

// Each `z.object(...)` below is `.strict()` so that an unknown field on
// the wire (e.g. an envelope or event payload with a new key the
// receiver does not yet know about) is rejected at the IPC boundary
// rather than silently stripped. Stripping at this seam is the failure
// mode where a producer "thinks" it sent a field that the consumer
// "thinks" it received — strict rejection turns that into a parse error
// with a helpful path.
//
// The event-payload arms inline the shared `taskId`/`atIso` fields
// rather than building the schema as `z.object({taskId, atIso}).strict()
// .and(z.discriminatedUnion("kind", ...))`: with strict on both sides
// of an intersection, zod parses the same input against each side and
// each arm rejects the other arm's keys as "unrecognized". Inlining
// keeps strict mode meaningful and lets each arm authoritatively list
// its full key set.
const eventPayloadCommonShape = {
  taskId: z.string().min(1),
  atIso: z.string().min(1),
} as const;

const eventPayloadSchema: z.ZodType<EventPayload, z.ZodTypeDef, unknown> = z
  .discriminatedUnion("kind", [
    z.object({
      ...eventPayloadCommonShape,
      kind: z.literal("state-changed"),
      data: stateChangedDataSchema,
    }).strict(),
    z.object({
      ...eventPayloadCommonShape,
      kind: z.literal("log"),
      data: logDataSchema,
    }).strict(),
    z.object({
      ...eventPayloadCommonShape,
      kind: z.literal("agent-message"),
      data: agentMessageDataSchema,
    }).strict(),
    z.object({
      ...eventPayloadCommonShape,
      kind: z.literal("github-call"),
      data: githubCallDataSchema,
    }).strict(),
    z.object({
      ...eventPayloadCommonShape,
      kind: z.literal("error"),
      data: errorDataSchema,
    }).strict(),
  ]);

const messageEnvelopeSchema: z.ZodType<MessageEnvelope, z.ZodTypeDef, unknown> = z
  .discriminatedUnion("type", [
    z.object({ id: messageIdSchema, type: z.literal("ping"), payload: pingPayloadSchema })
      .strict(),
    z.object({
      id: messageIdSchema,
      type: z.literal("subscribe"),
      payload: subscribePayloadSchema,
    }).strict(),
    z.object({
      id: messageIdSchema,
      type: z.literal("unsubscribe"),
      payload: unsubscribePayloadSchema,
    }).strict(),
    z.object({
      id: messageIdSchema,
      type: z.literal("command"),
      payload: commandPayloadSchema,
    }).strict(),
    z.object({
      id: messageIdSchema,
      type: z.literal("prompt"),
      payload: promptPayloadSchema,
    }).strict(),
    z.object({ id: messageIdSchema, type: z.literal("pong"), payload: pongPayloadSchema })
      .strict(),
    z.object({ id: messageIdSchema, type: z.literal("ack"), payload: ackPayloadSchema })
      .strict(),
    z.object({ id: messageIdSchema, type: z.literal("event"), payload: eventPayloadSchema })
      .strict(),
  ]);

// ---------------------------------------------------------------------------
// Public parse API
// ---------------------------------------------------------------------------

/**
 * Parse an arbitrary JSON value as a {@link MessageEnvelope}. Returns
 * either `{ success: true, data }` or `{ success: false, error }` so
 * callers can handle validation failures without `try`/`catch`.
 *
 * The codec calls this on every decoded frame; consumers typically do
 * not need to call it directly.
 *
 * @param raw The candidate value (typically the result of `JSON.parse`).
 * @returns The parsed envelope or the zod error.
 *
 * @example
 * ```ts
 * import { parseEnvelope } from "./protocol.ts";
 *
 * const result = parseEnvelope({ id: "1", type: "ping", payload: {} });
 * if (result.success) {
 *   handle(result.data);
 * } else {
 *   throw new Error(result.issues[0]?.message);
 * }
 * ```
 */
export function parseEnvelope(raw: unknown): ParseEnvelopeResult {
  const result = messageEnvelopeSchema.safeParse(raw);
  if (result.success) {
    return { success: true, data: result.data };
  }
  const issues: EnvelopeValidationIssue[] = result.error.issues.map((issue) => ({
    path: [...issue.path],
    message: issue.message,
    code: issue.code,
  }));
  return { success: false, issues };
}
