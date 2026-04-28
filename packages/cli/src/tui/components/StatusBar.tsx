/**
 * tui/components/StatusBar.tsx — bottom pane of the TUI.
 *
 * Renders a single line of status text — the most recent log message
 * from the daemon, the most recent error (if any), and a tiny
 * keybindings hint. Stateless; everything is driven through props so
 * the snapshot tests can exercise every variant deterministically.
 *
 * @module
 */

import { Box, Text } from "ink";
import type { ReactElement } from "react";

/**
 * Props accepted by {@link StatusBar}.
 */
export interface StatusBarProps {
  /**
   * Most recent informational message to display on the left. Falls
   * back to a static placeholder when omitted.
   */
  readonly message?: string | undefined;
  /**
   * Most recent error message; renders red when present and supersedes
   * {@link StatusBarProps.message} visually.
   */
  readonly errorMessage?: string | undefined;
  /**
   * Keybindings hint shown on the right. Wave 3 wires the command
   * palette and task switcher and updates this string from above.
   */
  readonly keybindingsHint?: string | undefined;
}

/**
 * Render the TUI status bar.
 *
 * @param props See {@link StatusBarProps}.
 * @returns The status-bar element.
 *
 * @example
 * ```tsx
 * <StatusBar message="ready" />
 * ```
 */
export function StatusBar(props: StatusBarProps): ReactElement {
  const {
    message,
    errorMessage,
    keybindingsHint = "Ctrl+C exit · / command palette (W3)",
  } = props;
  const left = errorMessage !== undefined
    ? <Text color="red">{`error: ${errorMessage}`}</Text>
    : <Text>{message ?? "ready"}</Text>;
  return (
    <Box flexDirection="row" justifyContent="space-between" paddingX={1}>
      {left}
      <Text dimColor>{keybindingsHint}</Text>
    </Box>
  );
}
