/**
 * tui/components/Header.tsx — top pane of the TUI.
 *
 * Renders the makina version, the daemon connection status, and the
 * focused-task id (when one is selected). Stateless; everything is
 * driven through props so the snapshot tests can reproduce every
 * variant deterministically.
 *
 * @module
 */

import { Box, Text } from "ink";
import type { ReactElement } from "react";

import type { DaemonConnectionStatus } from "../hooks/useDaemonConnection.ts";
import type { TaskId } from "../../types.ts";

/**
 * Props accepted by {@link Header}.
 */
export interface HeaderProps {
  /** The makina version to display in the top-left. */
  readonly version: string;
  /** Daemon connection status; affects the status pill colour. */
  readonly status: DaemonConnectionStatus;
  /** The currently-focused task id, when one exists. */
  readonly focusedTaskId?: TaskId | undefined;
}

/**
 * Render the TUI header.
 *
 * @param props See {@link HeaderProps}.
 * @returns The header element.
 *
 * @example
 * ```tsx
 * <Header version="0.0.0-dev" status="connected" />
 * ```
 */
export function Header(props: HeaderProps): ReactElement {
  const { version, status, focusedTaskId } = props;
  const focusLabel = focusedTaskId === undefined ? "no task focused" : `focus: ${focusedTaskId}`;
  return (
    <Box
      flexDirection="row"
      borderStyle="round"
      borderColor="cyan"
      paddingX={1}
      justifyContent="space-between"
    >
      <Text bold color="cyan">makina v{version}</Text>
      <Text color={statusColor(status)}>{statusLabel(status)}</Text>
      <Text dimColor>{focusLabel}</Text>
    </Box>
  );
}

/**
 * Map a connection status to a human-readable label.
 *
 * @param status The current status.
 * @returns The label rendered in the header pill.
 */
function statusLabel(status: DaemonConnectionStatus): string {
  switch (status) {
    case "idle":
      return "idle";
    case "connecting":
      return "connecting…";
    case "connected":
      return "connected";
    case "disconnected":
      return "disconnected";
    case "error":
      return "error";
  }
}

/**
 * Map a connection status to the colour applied to its label.
 *
 * @param status The current status.
 * @returns A colour name accepted by Ink's `<Text color>` prop.
 */
function statusColor(status: DaemonConnectionStatus): string {
  switch (status) {
    case "idle":
    case "connecting":
      return "yellow";
    case "connected":
      return "green";
    case "disconnected":
      return "gray";
    case "error":
      return "red";
  }
}
