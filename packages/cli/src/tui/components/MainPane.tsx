/**
 * tui/components/MainPane.tsx — middle pane of the TUI.
 *
 * Wave 2 ships the empty placeholder shell: the pane exists, the
 * snapshot tests assert that it renders, and the active-task count is
 * surfaced so the daemon's events are observable end-to-end. Wave 3
 * fills it with the task list, the per-task scrollback, and the
 * focused-task detail view.
 *
 * @module
 */

import { Box, Text } from "ink";
import type { ReactElement } from "react";

/**
 * Props accepted by {@link MainPane}.
 */
export interface MainPaneProps {
  /**
   * Number of tasks the daemon has reported so far. Drives the empty-
   * vs. populated copy.
   */
  readonly activeTaskCount: number;
  /**
   * Optional human-readable hint displayed under the headline (e.g.
   * "Connecting to daemon…").
   */
  readonly hint?: string | undefined;
}

/**
 * Render the TUI main pane.
 *
 * @param props See {@link MainPaneProps}.
 * @returns The main-pane element.
 *
 * @example
 * ```tsx
 * <MainPane activeTaskCount={0} />
 * ```
 */
export function MainPane(props: MainPaneProps): ReactElement {
  const { activeTaskCount, hint } = props;
  const headline = activeTaskCount === 0
    ? "No tasks yet — invoke /issue <repo>#<n> to start one."
    : `${activeTaskCount} ${activeTaskCount === 1 ? "task" : "tasks"} active.`;
  return (
    <Box
      flexGrow={1}
      flexDirection="column"
      borderStyle="round"
      borderColor="gray"
      paddingX={1}
      paddingY={1}
    >
      <Text>{headline}</Text>
      {hint !== undefined ? <Text dimColor>{hint}</Text> : null}
    </Box>
  );
}
