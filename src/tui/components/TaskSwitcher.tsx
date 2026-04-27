/**
 * tui/components/TaskSwitcher.tsx — overlay listing in-flight tasks.
 *
 * The user opens this overlay through the configured keybinding
 * (default `Ctrl+G`); arrow keys move the highlight, `Enter` focuses
 * the highlighted task, `Escape` closes the overlay without changing
 * focus.
 *
 * The component is **stateless about the daemon**: every piece of
 * task data flows in through props from the {@link useFocusedTask}
 * hook above. That keeps the snapshot tests trivially deterministic
 * and lets the App control its lifetime.
 *
 * @module
 */

import { Box, Text, useInput } from "ink";
import { type ReactElement, useCallback, useEffect, useState } from "react";

import { formatTaskAge, type TaskSummary } from "../hooks/useFocusedTask.ts";
import type { TaskId, TaskState } from "../../types.ts";

/**
 * Props accepted by {@link TaskSwitcher}.
 */
export interface TaskSwitcherProps {
  /** The tasks the daemon has reported, sorted most-recent-first. */
  readonly tasks: readonly TaskSummary[];
  /** The currently-focused task, when any. */
  readonly focusedTaskId?: TaskId | undefined;
  /**
   * Called when the user picks a task with `Enter`. The App routes
   * this to {@link FocusedTaskApi.focusTask} and closes the overlay.
   *
   * @param taskId The picked task's id.
   */
  readonly onPick: (taskId: TaskId) => void;
  /** Called when the user dismisses the overlay (`Escape`). */
  readonly onClose: () => void;
  /**
   * If `true`, Ink's `useInput` is wired up. Defaults to `true`. The
   * snapshot tests render the overlay with `inputEnabled={false}` so
   * Ink's stdin reader does not race the Deno test sanitizer.
   *
   * @default true
   */
  readonly inputEnabled?: boolean | undefined;
  /**
   * Reference timestamp passed to {@link formatTaskAge}. Defaults to
   * `new Date()`; the snapshot tests pin a fixed clock to keep the
   * rendered ages stable.
   */
  readonly now?: Date | undefined;
}

/**
 * Render the task switcher overlay.
 *
 * Stateful only on the cursor index. Synchronises the cursor with the
 * focused task on first render so the highlight starts on the
 * currently-focused task.
 *
 * @param props See {@link TaskSwitcherProps}.
 * @returns The overlay element.
 *
 * @example
 * ```tsx
 * <TaskSwitcher
 *   tasks={tasks}
 *   focusedTaskId={focused}
 *   onPick={focusTask}
 *   onClose={() => setOpen(false)}
 * />
 * ```
 */
export function TaskSwitcher(props: TaskSwitcherProps): ReactElement {
  const {
    tasks,
    focusedTaskId,
    onPick,
    onClose,
    inputEnabled = true,
    now,
  } = props;
  const [cursor, setCursor] = useState(() => initialCursor(tasks, focusedTaskId));

  // Re-pin the cursor when the task list shrinks below the cursor
  // index. Without this guard, removing the last task would leave the
  // cursor pointing at -1 (logical) and crash the keyboard handler.
  useEffect(() => {
    if (tasks.length === 0) {
      setCursor(0);
      return;
    }
    if (cursor >= tasks.length) {
      setCursor(tasks.length - 1);
    }
  }, [tasks.length, cursor]);

  const handleInput = useCallback(
    (
      input: string,
      key: { upArrow: boolean; downArrow: boolean; return: boolean; escape: boolean },
    ) => {
      if (key.escape) {
        onClose();
        return;
      }
      if (tasks.length === 0) {
        // Nothing to navigate; ignore arrow keys but let `Enter`
        // close so the user is not stuck with an empty list.
        if (key.return) {
          onClose();
        }
        return;
      }
      if (key.upArrow) {
        setCursor((previous) => (previous <= 0 ? tasks.length - 1 : previous - 1));
        return;
      }
      if (key.downArrow) {
        setCursor((previous) => (previous + 1) % tasks.length);
        return;
      }
      if (key.return) {
        const picked = tasks[cursor];
        if (picked !== undefined) {
          onPick(picked.id);
        }
        return;
      }
      // Vi-style fallback so the switcher still works in environments
      // where the arrow keys do not propagate clean key codes.
      if (input === "j") {
        setCursor((previous) => (previous + 1) % tasks.length);
      } else if (input === "k") {
        setCursor((previous) => (previous <= 0 ? tasks.length - 1 : previous - 1));
      }
    },
    [cursor, onClose, onPick, tasks],
  );

  // Wire the keyboard handler under Ink's `useInput` only when input
  // is enabled. The hook must always be called for React's hook order
  // contract; it falls through to `noop` when `inputEnabled` is false.
  useInput(
    (input, key) => {
      if (!inputEnabled) {
        return;
      }
      handleInput(input, key);
    },
    { isActive: inputEnabled },
  );

  return (
    <Box
      flexDirection="column"
      borderStyle="round"
      borderColor="magenta"
      paddingX={1}
      paddingY={0}
    >
      <Box justifyContent="space-between">
        <Text bold color="magenta">Task Switcher</Text>
        <Text dimColor>↑/↓ navigate · Enter focus · Esc close</Text>
      </Box>
      {tasks.length === 0
        ? <Text dimColor>No tasks yet — invoke /issue to start one.</Text>
        : tasks.map((task, index) => (
          <TaskRow
            key={task.id}
            task={task}
            highlighted={index === cursor}
            focused={task.id === focusedTaskId}
            now={now}
          />
        ))}
    </Box>
  );
}

/**
 * Compute the cursor's initial position so it lands on the focused
 * task when one exists, falling back to the first row otherwise.
 *
 * @param tasks Task list.
 * @param focusedTaskId Currently-focused task, if any.
 * @returns The initial cursor index (>= 0).
 */
function initialCursor(
  tasks: readonly TaskSummary[],
  focusedTaskId: TaskId | undefined,
): number {
  if (focusedTaskId === undefined) {
    return 0;
  }
  const index = tasks.findIndex((task) => task.id === focusedTaskId);
  return index < 0 ? 0 : index;
}

/**
 * Single row in the switcher. Stateless; everything flows in via
 * props so the snapshot tests can pin every variant deterministically.
 */
interface TaskRowProps {
  /** The task summary to render. */
  readonly task: TaskSummary;
  /** Whether the cursor currently sits on this row. */
  readonly highlighted: boolean;
  /** Whether the task is the currently-focused one. */
  readonly focused: boolean;
  /** Reference clock, forwarded to {@link formatTaskAge}. */
  readonly now: Date | undefined;
}

/**
 * Render one row of the task switcher.
 *
 * @param props See {@link TaskRowProps}.
 * @returns The row element.
 */
function TaskRow(props: TaskRowProps): ReactElement {
  const { task, highlighted, focused, now } = props;
  const cursorMark = highlighted ? ">" : " ";
  const focusMark = focused ? "*" : " ";
  const age = formatTaskAge(task, now);
  const stateText = stateBadge(task.state);
  const colour = stateColor(task.state);
  return (
    <Box flexDirection="row">
      {highlighted
        ? (
          <Text color="magenta">
            {cursorMark} {focusMark} {task.id}
          </Text>
        )
        : (
          <Text>
            {cursorMark} {focusMark} {task.id}
          </Text>
        )}
      <Box flexGrow={1} />
      {colour === undefined ? <Text>{stateText}</Text> : <Text color={colour}>{stateText}</Text>}
      <Text dimColor>{` ${age}`}</Text>
    </Box>
  );
}

/**
 * Map a task state to a short uppercase badge.
 *
 * @param state Current task state, when known.
 * @returns The badge text.
 */
function stateBadge(state: TaskState | undefined): string {
  if (state === undefined) {
    return "PENDING";
  }
  return state;
}

/**
 * Map a task state to the colour applied to its badge.
 *
 * @param state Current task state, when known.
 * @returns A colour name accepted by Ink's `<Text color>` prop.
 */
function stateColor(state: TaskState | undefined): string | undefined {
  switch (state) {
    case undefined:
      return "gray";
    case "INIT":
    case "CLONING_WORKTREE":
    case "DRAFTING":
    case "COMMITTING":
    case "PUSHING":
      return "yellow";
    case "PR_OPEN":
    case "STABILIZING":
      return "cyan";
    case "READY_TO_MERGE":
      return "green";
    case "MERGED":
      return "green";
    case "NEEDS_HUMAN":
      return "yellow";
    case "FAILED":
      return "red";
  }
}
