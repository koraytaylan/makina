/**
 * tui/components/CommandPalette.tsx — overlay that captures user
 * input and dispatches it as a slash command.
 *
 * Default toggle: `Ctrl+P` (configurable via `tui.keybindings.commandPalette`
 * in `config.json`). When open, the overlay renders a single-line
 * input field plus an autocomplete dropdown filtered against
 * {@link "../slash-command-parser.ts".SLASH_COMMAND_SPECS}; `Enter`
 * parses the line and forwards the resulting
 * {@link "../../ipc/protocol.ts".CommandPayload} to {@link CommandPaletteProps.onSubmit}.
 *
 * History is kept in-memory only — Wave 3 does not persist it across
 * runs (#14 explicitly out-of-scope). Up/Down without a typed prefix
 * walks the history; once the user types, Up/Down navigates the
 * suggestion dropdown instead.
 *
 * @module
 */

import { Box, Text, useInput } from "ink";
import { type ReactElement, useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  parseSlashCommand,
  SLASH_COMMAND_SPECS,
  SlashCommandParseError,
  type SlashCommandSpec,
} from "../slash-command-parser.ts";
import type { CommandPayload } from "../../ipc/protocol.ts";
import {
  COMMAND_PALETTE_HISTORY_LIMIT,
  COMMAND_PALETTE_SUGGESTION_WIDTH_CODE_UNITS,
} from "../../constants.ts";

/**
 * Props accepted by {@link CommandPalette}.
 */
export interface CommandPaletteProps {
  /**
   * Called when the user submits a parsed command. The App routes the
   * payload through the daemon connection and closes the overlay.
   *
   * @param payload The parsed command payload.
   * @param raw The raw user input that produced it (with surrounding
   *   whitespace trimmed). Useful for the App-level history log.
   */
  readonly onSubmit: (payload: CommandPayload, raw: string) => void;
  /** Called when the user dismisses the overlay (`Escape`). */
  readonly onClose: () => void;
  /**
   * Optional history seed. The overlay defensively copies and trims
   * to {@link COMMAND_PALETTE_HISTORY_LIMIT}. Most-recent first.
   */
  readonly history?: readonly string[] | undefined;
  /**
   * If `true`, Ink's `useInput` is wired up. The snapshot tests render
   * the overlay with `inputEnabled={false}` so Ink's stdin reader
   * does not race the Deno test sanitizer.
   *
   * @default true
   */
  readonly inputEnabled?: boolean | undefined;
  /**
   * Initial input value, useful for the snapshot tests' "filtered"
   * variant. Defaults to `""`.
   */
  readonly initialInput?: string | undefined;
}

/**
 * Render the command-palette overlay.
 *
 * @param props See {@link CommandPaletteProps}.
 * @returns The overlay element.
 *
 * @example
 * ```tsx
 * <CommandPalette
 *   onSubmit={(payload) => sendCommand(payload)}
 *   onClose={() => setOpen(false)}
 *   history={paletteHistory}
 * />
 * ```
 */
export function CommandPalette(props: CommandPaletteProps): ReactElement {
  const {
    onSubmit,
    onClose,
    history = [],
    inputEnabled = true,
    initialInput = "",
  } = props;
  const [input, setInput] = useState(initialInput);
  const [suggestionIndex, setSuggestionIndex] = useState(0);
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [parseError, setParseError] = useState<string | undefined>(undefined);

  // Trim and de-duplicate the seeded history so the up/down navigation
  // never replays the same line twice in a row.
  const trimmedHistory = useMemo(() => {
    return dedupe(history).slice(0, COMMAND_PALETTE_HISTORY_LIMIT);
  }, [history]);

  // Filter the spec list to entries whose name shares a prefix with
  // the current input (with the leading `/` stripped). When the input
  // is empty, every spec is shown.
  const suggestions = useMemo<readonly SlashCommandSpec[]>(() => {
    return filterSuggestions(input);
  }, [input]);

  // Re-pin the suggestion cursor whenever the suggestion list shrinks.
  useEffect(() => {
    if (suggestionIndex >= suggestions.length) {
      setSuggestionIndex(suggestions.length === 0 ? 0 : suggestions.length - 1);
    }
  }, [suggestionIndex, suggestions.length]);

  const handleSubmit = useCallback(() => {
    if (input.trim().length === 0) {
      return;
    }
    // The overlay renders a leading `/` placeholder when `input` is
    // empty, but `input` itself is the source of truth. If the user
    // types the command name without an explicit `/` (relying on the
    // visual hint) reconstruct the canonical `/<name>` form before
    // parsing so the captured payload matches what the user saw on
    // screen. A user who typed an explicit `/` keeps it verbatim.
    const trimmed = input.trim();
    const canonical = trimmed.startsWith("/") ? trimmed : `/${trimmed}`;
    try {
      const payload = parseSlashCommand(canonical);
      setParseError(undefined);
      onSubmit(payload, canonical);
    } catch (error) {
      if (error instanceof SlashCommandParseError) {
        setParseError(error.message);
        return;
      }
      throw error;
    }
  }, [input, onSubmit]);

  // Stable input ref so the keyboard handler can read the current
  // value without re-binding on every keystroke.
  const inputRef = useRef(input);
  useEffect(() => {
    inputRef.current = input;
  }, [input]);

  // Draft retained across history navigation. The first time the user
  // walks into history we snapshot the in-progress input; when they
  // walk back above the freshest entry we restore it. The ref is reset
  // whenever the user types a fresh character (handleInputChange) so a
  // brand-new input does not leak the previous draft.
  const draftRef = useRef<string | null>(null);

  const applyHistory = useCallback(
    (delta: 1 | -1) => {
      if (trimmedHistory.length === 0) {
        return;
      }
      setHistoryIndex((previous) => {
        const start = previous ?? -1;
        if (previous === null && delta === 1) {
          // Entering history: capture the current input so we can
          // restore it if the user walks back above the freshest entry.
          draftRef.current = inputRef.current;
        }
        const candidate = start + delta;
        if (candidate < 0) {
          // Walked above the freshest entry — restore the in-progress
          // input the user had before they started navigating.
          if (draftRef.current !== null) {
            setInput(draftRef.current);
            draftRef.current = null;
          }
          return null;
        }
        const clamped = Math.min(candidate, trimmedHistory.length - 1);
        const replacement = trimmedHistory[clamped];
        if (replacement !== undefined) {
          setInput(replacement);
        }
        return clamped;
      });
    },
    [trimmedHistory],
  );

  const handleInputChange = useCallback((next: string) => {
    setInput(next);
    setHistoryIndex(null);
    setSuggestionIndex(0);
    setParseError(undefined);
    // The user is editing a fresh draft now; drop any cached pre-history
    // snapshot so a future history walk captures the new value.
    draftRef.current = null;
  }, []);

  // Decide whether arrow keys walk history or autocomplete. Two
  // conditions keep arrows on the history rail: the input field is
  // empty (a fresh palette open), or the user is already in the middle
  // of a history walk (so the recalled command does not flip arrows
  // over to the suggestion dropdown). Once the user types a fresh
  // character `handleInputChange` clears `historyIndex` and arrows
  // switch back to navigating suggestions.
  const arrowsNavigateHistory = historyIndex !== null || input.trim().length === 0;

  useInput(
    (rawInput, key) => {
      if (!inputEnabled) {
        return;
      }
      if (key.escape) {
        onClose();
        return;
      }
      if (key.return) {
        handleSubmit();
        return;
      }
      if (key.upArrow) {
        if (arrowsNavigateHistory) {
          applyHistory(1);
          return;
        }
        if (suggestions.length > 0) {
          setSuggestionIndex((previous) => previous <= 0 ? suggestions.length - 1 : previous - 1);
        }
        return;
      }
      if (key.downArrow) {
        if (arrowsNavigateHistory) {
          applyHistory(-1);
          return;
        }
        if (suggestions.length > 0) {
          setSuggestionIndex((previous) => (previous + 1) % suggestions.length);
        }
        return;
      }
      if (key.tab) {
        // Tab inserts the highlighted suggestion's canonical
        // `/<name>` form. The user can keep typing arguments after.
        const chosen = suggestions[suggestionIndex];
        if (chosen !== undefined) {
          handleInputChange(`/${chosen.name} `);
        }
        return;
      }
      if (key.backspace || key.delete) {
        if (inputRef.current.length === 0) {
          return;
        }
        handleInputChange(inputRef.current.slice(0, -1));
        return;
      }
      if (rawInput.length > 0 && !key.ctrl && !key.meta) {
        handleInputChange(`${inputRef.current}${rawInput}`);
      }
    },
    { isActive: inputEnabled },
  );

  return (
    <Box
      flexDirection="column"
      borderStyle="round"
      borderColor="cyan"
      paddingX={1}
      paddingY={0}
    >
      <Box justifyContent="space-between">
        <Text bold color="cyan">Command Palette</Text>
        <Text dimColor>Tab autocomplete · Enter run · Esc close</Text>
      </Box>
      <Box>
        <Text color="cyan">{"> "}</Text>
        <Text>{input.length === 0 ? "/" : input}</Text>
      </Box>
      {parseError !== undefined
        ? <Text color="red">{parseError}</Text>
        : suggestions.length === 0
        ? <Text dimColor>No matching commands.</Text>
        : (
          <Box flexDirection="column">
            {suggestions.map((spec, index) => (
              <SuggestionRow
                key={spec.name}
                spec={spec}
                highlighted={index === suggestionIndex}
              />
            ))}
          </Box>
        )}
      <Box>
        {trimmedHistory.length > 0
          ? (
            <Text dimColor>
              History: {trimmedHistory.length} ↑/↓ recall
            </Text>
          )
          : <Text dimColor>No history yet.</Text>}
      </Box>
    </Box>
  );
}

/**
 * Filter {@link SLASH_COMMAND_SPECS} to entries whose name shares a
 * prefix with the current input. The input may or may not start with
 * `/`; both shapes filter identically.
 *
 * Public so the snapshot tests can drive deterministic dropdowns
 * without going through `useState`.
 *
 * @param input Current palette input.
 * @returns The filtered specs, in the source order
 *   ({@link SLASH_COMMAND_SPECS} is alphabetical).
 */
export function filterSuggestions(input: string): readonly SlashCommandSpec[] {
  const trimmed = input.trim();
  const stem = trimmed.startsWith("/") ? trimmed.slice(1) : trimmed;
  if (stem.length === 0) {
    return SLASH_COMMAND_SPECS;
  }
  // Match the head token only — once the user has typed past the name
  // (`/issue 4` etc.) the dropdown narrows to the matching spec.
  const head = stem.split(/\s+/u, 1)[0] ?? "";
  return SLASH_COMMAND_SPECS.filter((spec) => spec.name.startsWith(head));
}

/**
 * Drop duplicates from `entries`, preserving order. Empty strings are
 * dropped too — the history is round-robined by trimming the head;
 * an empty entry would compare equal to nothing.
 *
 * @param entries The candidate entries.
 * @returns The de-duplicated list.
 */
function dedupe(entries: readonly string[]): readonly string[] {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const entry of entries) {
    const trimmed = entry.trim();
    if (trimmed.length === 0) continue;
    if (seen.has(trimmed)) continue;
    seen.add(trimmed);
    result.push(trimmed);
  }
  return result;
}

/**
 * Render one entry in the suggestion dropdown.
 */
interface SuggestionRowProps {
  /** The spec to render. */
  readonly spec: SlashCommandSpec;
  /** Whether the cursor sits on this entry. */
  readonly highlighted: boolean;
}

/**
 * Render one suggestion row.
 *
 * @param props See {@link SuggestionRowProps}.
 * @returns The row element.
 */
function SuggestionRow(props: SuggestionRowProps): ReactElement {
  const { spec, highlighted } = props;
  const cursor = highlighted ? ">" : " ";
  const text = `${cursor} /${spec.name} — ${spec.summary}`;
  if (highlighted) {
    return (
      <Text color="cyan">
        {truncate(text, COMMAND_PALETTE_SUGGESTION_WIDTH_CODE_UNITS)}
      </Text>
    );
  }
  return <Text>{truncate(text, COMMAND_PALETTE_SUGGESTION_WIDTH_CODE_UNITS)}</Text>;
}

/**
 * Truncate `text` to at most `limit` UTF-16 code units, appending an
 * ellipsis when shortened. Centralised here so the dropdown rows do
 * not need to import the App-level truncator.
 *
 * @param text The text to truncate.
 * @param limit The maximum length, in UTF-16 code units.
 * @returns The (possibly-truncated) string.
 */
function truncate(text: string, limit: number): string {
  if (text.length <= limit) {
    return text;
  }
  return `${text.slice(0, Math.max(0, limit - 1))}…`;
}
