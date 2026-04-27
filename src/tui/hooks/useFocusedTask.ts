/**
 * tui/hooks/useFocusedTask.ts — focused-task state plus event subscription
 * helpers for the Wave 3 overlays.
 *
 * The Wave 2 shell tracked a single focused task id inline in
 * `App.tsx`; Wave 3 spreads that responsibility across the command
 * palette and the task switcher, so the state has been lifted into a
 * dedicated hook. {@link useFocusedTask} centralises three concerns:
 *
 * 1. Maintaining a per-task index of the most recent
 *    {@link TaskSummary} (state + last activity timestamp), built up
 *    from {@link DaemonClient} events.
 * 2. Tracking which task is "focused" right now — initially the first
 *    task the daemon reports about, switchable through
 *    {@link FocusedTaskApi.focusTask}.
 * 3. Exposing a stable accessor surface so consumer components (the
 *    task switcher, the header, the future per-task scrollback) can
 *    re-render without flapping.
 *
 * The hook is transport-agnostic: it accepts the `subscribe` half of
 * the {@link DaemonClient} surface (the same shape `useDaemonConnection`
 * exposes) so the snapshot tests can drive it with the in-memory
 * double.
 *
 * @module
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { EventPayload } from "../../ipc/protocol.ts";
import { makeTaskId, type TaskEvent, type TaskId, type TaskState } from "../../types.ts";
import {
  HOURS_PER_DAY,
  MILLISECONDS_PER_SECOND,
  MINUTES_PER_HOUR,
  SECONDS_PER_MINUTE,
} from "../../constants.ts";

/**
 * Snapshot of one task as the TUI knows it. Built up from the events
 * the daemon publishes.
 *
 * - `id` — the task identifier the daemon assigned.
 * - `state` — the most recent {@link TaskState} reported. `undefined`
 *   when the only events seen so far are `log`/`agent-message`/etc.
 *   without a state-change.
 * - `firstSeenAtIso` — wall-clock ISO timestamp of the *first* event
 *   the TUI saw for this task. Used by the task switcher's age
 *   formatter.
 * - `lastSeenAtIso` — wall-clock ISO timestamp of the most recent
 *   event. Used by the task switcher to sort by activity.
 */
export interface TaskSummary {
  /** Task identifier. */
  readonly id: TaskId;
  /** Most-recent task state, when one has been observed. */
  readonly state: TaskState | undefined;
  /** ISO timestamp of the first observed event for this task. */
  readonly firstSeenAtIso: string;
  /** ISO timestamp of the most recent observed event for this task. */
  readonly lastSeenAtIso: string;
}

/**
 * The callback signature {@link FocusedTaskApi.subscribe} expects.
 * Mirrors the {@link "../ipc-client.ts".DaemonEventSubscription} surface
 * the in-memory double also exposes.
 */
export interface FocusedTaskEventSubscription {
  /** Stop forwarding events to this handler. */
  unsubscribe(): void;
}

/**
 * Public surface exposed by {@link useFocusedTask}.
 *
 * Stable across re-renders: the function references survive every
 * render commit so consumers can include them in dependency arrays
 * without triggering effect loops.
 */
export interface FocusedTaskApi {
  /**
   * The currently-focused task id, when one exists. Set the first time
   * the hook hears about a task (mirroring Wave 2's auto-focus
   * behaviour) or whenever the user invokes
   * {@link FocusedTaskApi.focusTask}.
   */
  readonly focusedTaskId: TaskId | undefined;
  /**
   * Snapshot of every task the hook has observed, in stable order
   * (most-recent activity first).
   */
  readonly tasks: readonly TaskSummary[];
  /**
   * Switch focus to `taskId`. The id does not have to be present in
   * {@link FocusedTaskApi.tasks} — focusing a task before the first
   * event arrives is allowed (the switcher renders the focus pill
   * either way).
   *
   * @param taskId The task to focus.
   */
  readonly focusTask: (taskId: TaskId) => void;
  /** Drop the current focus (the header reverts to "no task focused"). */
  readonly clearFocus: () => void;
}

/**
 * The slice of {@link "../ipc-client.ts".DaemonClient} the hook needs.
 *
 * Either a real socket-backed client or the in-memory test double
 * works here: both expose `subscribeEvents` with the same signature.
 */
export interface FocusedTaskEventSource {
  /**
   * Subscribe to event envelopes pushed by the daemon. The handler is
   * invoked synchronously per event in arrival order.
   *
   * @param handler The callback invoked per event.
   * @returns A subscription handle whose `unsubscribe()` stops further
   *   deliveries.
   */
  subscribeEvents(
    handler: (event: EventPayload) => void,
  ): FocusedTaskEventSubscription;
}

/**
 * Options accepted by {@link useFocusedTask}.
 */
export interface UseFocusedTaskOptions {
  /**
   * Source of pushed task events. Pass either the {@link DaemonClient}
   * or any object that exposes the `subscribeEvents` surface (the
   * `useDaemonConnection` hook's return value works directly).
   */
  readonly source: FocusedTaskEventSource;
  /**
   * Optional initial focus. Useful for tests that want a deterministic
   * starting state without needing to inject an event first. Treated
   * as a one-time seed: the auto-focus on first-event behaviour is
   * skipped while this is set.
   */
  readonly initialFocusedTaskId?: TaskId | undefined;
}

/**
 * React hook that tracks the focused task and the per-task summary
 * index.
 *
 * Wave 3 components — the task switcher, the header's focus pill, the
 * focused-task scrollback — all call this hook (or read its API
 * through context) so the focus state is single-sourced. The hook
 * deliberately does **not** drive any IPC of its own: focusing a task
 * is a TUI-side concern; the supervisor on the daemon side is unaware
 * of which task the user is staring at.
 *
 * @param options See {@link UseFocusedTaskOptions}.
 * @returns The focused-task API; see {@link FocusedTaskApi}.
 *
 * @example
 * ```tsx
 * import { useFocusedTask } from "./useFocusedTask.ts";
 *
 * function Switcher({ client }: { client: DaemonClient }) {
 *   const { tasks, focusedTaskId, focusTask } = useFocusedTask({ source: client });
 *   return tasks.map((task) => (
 *     <Text key={task.id}>{task.id === focusedTaskId ? "* " : "  "}{task.id}</Text>
 *   ));
 * }
 * ```
 */
export function useFocusedTask(options: UseFocusedTaskOptions): FocusedTaskApi {
  const { source, initialFocusedTaskId } = options;
  const [focusedTaskId, setFocusedTaskId] = useState<TaskId | undefined>(
    initialFocusedTaskId,
  );
  const [taskMap, setTaskMap] = useState<ReadonlyMap<TaskId, TaskSummary>>(
    () => new Map(),
  );

  // Keep `taskMap`'s reference stable when no events have arrived; the
  // memo on `tasks` below depends on it, and producing a fresh empty
  // array per render would break consumer memoisation.
  const tasks = useMemo<readonly TaskSummary[]>(() => {
    return [...taskMap.values()].sort((left, right) => {
      // Sort by lastSeenAtIso descending — most recent activity first.
      // ISO strings compare lexicographically the same way they
      // compare temporally for the same calendar epoch.
      if (left.lastSeenAtIso === right.lastSeenAtIso) {
        return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
      }
      return left.lastSeenAtIso < right.lastSeenAtIso ? 1 : -1;
    });
  }, [taskMap]);

  // Stable callbacks — they read the focused-task id through the ref
  // below so they do not need to re-bind on every render.
  const focusTask = useCallback((taskId: TaskId) => {
    setFocusedTaskId(taskId);
  }, []);

  const clearFocus = useCallback(() => {
    setFocusedTaskId(undefined);
  }, []);

  // `sourceRef` mirrors the source so the subscribe effect can pick up
  // a fresh subscriber on remount without re-running on every render.
  const sourceRef = useRef(source);
  useEffect(() => {
    sourceRef.current = source;
  }, [source]);

  useEffect(() => {
    const subscription = source.subscribeEvents((event) => {
      handleFocusedTaskEvent(event, {
        setTaskMap,
        setFocusedTaskId,
      });
    });
    return () => {
      subscription.unsubscribe();
    };
  }, [source]);

  return { focusedTaskId, tasks, focusTask, clearFocus };
}

/**
 * Setters {@link handleFocusedTaskEvent} mutates when an event arrives.
 *
 * Extracted so the same code path is used by the production hook
 * (passing React `useState` setters) and by direct unit tests (passing
 * plain closures over local variables).
 */
export interface FocusedTaskEventSetters {
  /** Setter for the per-task summary index. */
  readonly setTaskMap: (
    update: (
      previous: ReadonlyMap<TaskId, TaskSummary>,
    ) => ReadonlyMap<TaskId, TaskSummary>,
  ) => void;
  /** Setter for the focused task id. */
  readonly setFocusedTaskId: (
    update: (previous: TaskId | undefined) => TaskId | undefined,
  ) => void;
}

/**
 * Apply a single pushed event to the focused-task state.
 *
 * Public for unit-test reuse; production code drives this through
 * {@link useFocusedTask}'s effect.
 *
 * @param event The event payload.
 * @param setters The state setters captured by {@link useFocusedTask}.
 */
export function handleFocusedTaskEvent(
  event: EventPayload,
  setters: FocusedTaskEventSetters,
): void {
  let taskId: TaskId;
  try {
    taskId = makeTaskId(event.taskId);
  } catch {
    // Match `App.tsx`'s tolerant handling: a malformed task id is not
    // worth crashing the renderer over. The focused-task hook simply
    // skips the event; `App.handleEvent` is still in charge of
    // surfacing the failure to the status bar.
    return;
  }
  setters.setTaskMap((previous) => {
    const existing = previous.get(taskId);
    const nextState = nextStateFromEvent(event, existing?.state);
    const summary: TaskSummary = {
      id: taskId,
      state: nextState,
      firstSeenAtIso: existing?.firstSeenAtIso ?? event.atIso,
      lastSeenAtIso: event.atIso,
    };
    if (existing !== undefined && summariesEqual(existing, summary)) {
      return previous;
    }
    const next = new Map(previous);
    next.set(taskId, summary);
    return next;
  });
  setters.setFocusedTaskId((previous) => previous ?? taskId);
}

/**
 * Extract the next {@link TaskState} from an event, falling back to
 * the previous value when the event does not carry a state-change.
 *
 * @param event The event payload.
 * @param previous The previously-known state for this task, when any.
 * @returns The next state or `undefined` if no state has been observed.
 */
function nextStateFromEvent(
  event: EventPayload,
  previous: TaskState | undefined,
): TaskState | undefined {
  if (event.kind === "state-changed") {
    return event.data.toState;
  }
  return previous;
}

/**
 * Compare two task summaries field-by-field to avoid re-rendering when
 * an event arrives that changes nothing observable (e.g. a duplicate
 * timestamp from a fan-out).
 *
 * @param left First summary.
 * @param right Second summary.
 * @returns `true` when the summaries match.
 */
function summariesEqual(left: TaskSummary, right: TaskSummary): boolean {
  return (
    left.id === right.id &&
    left.state === right.state &&
    left.firstSeenAtIso === right.firstSeenAtIso &&
    left.lastSeenAtIso === right.lastSeenAtIso
  );
}

/**
 * Compute the human-readable age of a {@link TaskSummary} relative to a
 * reference timestamp.
 *
 * The output is the longest still-meaningful unit (`5s`, `12m`, `3h`,
 * `2d`); both the command palette and the task switcher render this
 * verbatim. Ages below one second display as `0s` rather than empty.
 *
 * Public so the snapshot tests can pin the age strings deterministically
 * by passing an explicit `now` rather than relying on the wall clock.
 *
 * @param summary The task summary whose age to format.
 * @param now Reference timestamp (defaults to `new Date()`).
 * @returns The age string.
 */
export function formatTaskAge(summary: TaskSummary, now: Date = new Date()): string {
  return formatAgeBetween(summary.firstSeenAtIso, now);
}

/**
 * Compute the human-readable age between two timestamps.
 *
 * Used by both the {@link TaskSummary}-shaped helper above and any
 * call site that wants to format an arbitrary first-seen timestamp.
 *
 * @param fromIso ISO timestamp of the past event.
 * @param now Reference timestamp.
 * @returns The age string (`5s`, `12m`, `3h`, `2d`).
 */
export function formatAgeBetween(fromIso: string, now: Date): string {
  const fromDate = new Date(fromIso);
  if (Number.isNaN(fromDate.getTime())) {
    return "?";
  }
  const deltaMilliseconds = Math.max(0, now.getTime() - fromDate.getTime());
  return formatAgeMilliseconds(deltaMilliseconds);
}

/**
 * Convert a non-negative millisecond duration into the longest
 * still-meaningful single-unit string.
 *
 * @param deltaMilliseconds Non-negative duration.
 * @returns The age string (`5s`, `12m`, `3h`, `2d`).
 */
function formatAgeMilliseconds(deltaMilliseconds: number): string {
  const seconds = Math.floor(deltaMilliseconds / MILLISECONDS_PER_SECOND);
  if (seconds < SECONDS_PER_MINUTE) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / SECONDS_PER_MINUTE);
  if (minutes < MINUTES_PER_HOUR) {
    return `${minutes}m`;
  }
  const hours = Math.floor(minutes / MINUTES_PER_HOUR);
  if (hours < HOURS_PER_DAY) {
    return `${hours}h`;
  }
  const days = Math.floor(hours / HOURS_PER_DAY);
  return `${days}d`;
}

/**
 * Re-export so the matching shape on the daemon side
 * ({@link "../../types.ts".TaskEvent}) does not need to be imported
 * separately when consumers wire up listeners against this hook.
 */
export type { TaskEvent };
