/**
 * tui/keybindings.ts — chord parser shared by every TUI component.
 *
 * Config files store keybindings as `<modifier>+<key>` strings
 * (`ctrl+p`, `shift+tab`). Ink's `useInput` hands each keystroke to
 * the consumer as a `(input, key)` pair where `key` already
 * normalises modifier flags. This module bridges the two: turn a
 * chord string into a predicate that reports whether a given Ink
 * keystroke should fire it.
 *
 * The parser is intentionally cross-platform — modifiers are matched
 * by name (`ctrl`, `shift`, `meta`, `alt`) so a config file written
 * on macOS works on Linux without changes (Ink's `key.meta` flag
 * normalises across platforms).
 *
 * @module
 */

/**
 * Subset of Ink's `Key` shape this module reads. Pulled in as a
 * structural type rather than imported from Ink so unit tests can
 * call the helpers below with plain object literals.
 */
export interface KeystrokeFlags {
  /** `Ctrl` modifier was held. */
  readonly ctrl: boolean;
  /** `Shift` modifier was held. */
  readonly shift: boolean;
  /** `Meta` (Cmd / Super / Win) modifier was held. */
  readonly meta: boolean;
  /** Tab key. */
  readonly tab: boolean;
  /** Return / Enter key. */
  readonly return: boolean;
  /** Escape key. */
  readonly escape: boolean;
  /** Backspace key. */
  readonly backspace: boolean;
  /** Delete key. */
  readonly delete: boolean;
  /** Up arrow. */
  readonly upArrow: boolean;
  /** Down arrow. */
  readonly downArrow: boolean;
  /** Left arrow. */
  readonly leftArrow: boolean;
  /** Right arrow. */
  readonly rightArrow: boolean;
  /** Page-up. */
  readonly pageUp: boolean;
  /** Page-down. */
  readonly pageDown: boolean;
}

/**
 * Parsed representation of a keybinding chord.
 *
 * The `key` field carries the bare key name (e.g. `"p"`, `"tab"`,
 * `"escape"`). Modifier booleans report whether the chord requires
 * each modifier to be held.
 */
export interface ParsedKeybinding {
  /** `ctrl` modifier required. */
  readonly ctrl: boolean;
  /** `shift` modifier required. */
  readonly shift: boolean;
  /** `meta` modifier required. */
  readonly meta: boolean;
  /** `alt` modifier required (synonym for `meta` on most terminals). */
  readonly alt: boolean;
  /** Bare key (lowercased; named keys preserved literally). */
  readonly key: string;
}

/**
 * Error raised by {@link parseKeybinding} when the chord string
 * cannot be parsed.
 */
export class KeybindingParseError extends Error {
  /**
   * Construct a parse error.
   *
   * @param message Human-readable description.
   */
  constructor(message: string) {
    super(message);
    this.name = "KeybindingParseError";
  }
}

/**
 * Recognised modifier names. Lowercased for compare-insensitive
 * lookup.
 */
const MODIFIER_NAMES = new Set(["ctrl", "shift", "meta", "alt"]);

/**
 * Parse a chord string like `"ctrl+p"` into a {@link ParsedKeybinding}.
 *
 * Whitespace around the chord is trimmed; the components are split on
 * `+` and lowercased. Empty components and unknown modifier names are
 * rejected with {@link KeybindingParseError}.
 *
 * @param chord The chord string from `tui.keybindings`.
 * @returns The parsed structure.
 * @throws {KeybindingParseError} If the chord is malformed.
 *
 * @example
 * ```ts
 * const parsed = parseKeybinding("ctrl+p");
 * // → { ctrl: true, shift: false, meta: false, alt: false, key: "p" }
 * ```
 */
export function parseKeybinding(chord: string): ParsedKeybinding {
  const trimmed = chord.trim();
  if (trimmed.length === 0) {
    throw new KeybindingParseError("keybinding cannot be empty");
  }
  const parts = trimmed.toLowerCase().split("+").map((part) => part.trim());
  if (parts.some((part) => part.length === 0)) {
    throw new KeybindingParseError(
      `keybinding ${JSON.stringify(chord)} contains an empty component`,
    );
  }
  const key = parts[parts.length - 1];
  if (key === undefined) {
    throw new KeybindingParseError(
      `keybinding ${JSON.stringify(chord)} is missing a key`,
    );
  }
  if (MODIFIER_NAMES.has(key)) {
    throw new KeybindingParseError(
      `keybinding ${JSON.stringify(chord)} ends in a modifier name`,
    );
  }
  const modifiers = parts.slice(0, -1);
  for (const modifier of modifiers) {
    if (!MODIFIER_NAMES.has(modifier)) {
      throw new KeybindingParseError(
        `keybinding ${JSON.stringify(chord)} contains unknown modifier ${JSON.stringify(modifier)}`,
      );
    }
  }
  return {
    ctrl: modifiers.includes("ctrl"),
    shift: modifiers.includes("shift"),
    meta: modifiers.includes("meta"),
    alt: modifiers.includes("alt"),
    key,
  };
}

/**
 * Map of named keys to the corresponding flag on {@link KeystrokeFlags}.
 */
const NAMED_KEYS: Readonly<Record<string, keyof KeystrokeFlags>> = {
  tab: "tab",
  return: "return",
  enter: "return",
  escape: "escape",
  esc: "escape",
  backspace: "backspace",
  delete: "delete",
  up: "upArrow",
  down: "downArrow",
  left: "leftArrow",
  right: "rightArrow",
  pageup: "pageUp",
  pagedown: "pageDown",
};

/**
 * Test whether a keystroke matches a chord string.
 *
 * **Modifier flags must match exactly.** A chord without `shift+` does
 * not fire when the user is holding shift, and a `ctrl+shift+x` chord
 * does not fire on a plain `ctrl+x` keystroke. The bare key compares
 * case-insensitively against the typed character (or against
 * {@link KeystrokeFlags}'s named-key flags for special keys).
 *
 * **Ctrl+letter terminal quirks.** Many terminals deliver `Ctrl+A`
 * through `Ctrl+Z` as the corresponding ASCII control byte (`\x01`..
 * `\x1A`) instead of the bare letter; some Ink configurations leave
 * `input` empty altogether. The implementation accepts three shapes:
 * the literal letter, the ASCII control byte, or empty input plus a
 * single-character chord key. The empty-input branch is intentionally
 * permissive — it cannot disambiguate between two `ctrl+<letter>`
 * bindings — but the control-byte branch keeps a `ctrl+p` chord from
 * matching a `ctrl+,` keystroke on standards-conforming terminals.
 *
 * @param chord The chord string from the config.
 * @param input The character Ink reports for the keystroke.
 * @param flags The flag bag Ink hands the consumer.
 * @returns `true` when the keystroke fires the chord.
 *
 * @example
 * ```ts
 * useInput((input, key) => {
 *   if (matchesKeybinding("ctrl+p", input, key)) {
 *     setPaletteOpen(true);
 *   }
 * });
 * ```
 */
export function matchesKeybinding(
  chord: string,
  input: string,
  flags: KeystrokeFlags,
): boolean {
  let parsed: ParsedKeybinding;
  try {
    parsed = parseKeybinding(chord);
  } catch {
    return false;
  }
  if (parsed.ctrl !== flags.ctrl) return false;
  // `alt` is treated as the meta synonym Ink already exposes; the
  // chord-side flag is folded into `meta` so the modifier check below
  // matches the keystroke's flag bag uniformly.
  const requiresMeta = parsed.meta || parsed.alt;
  if (requiresMeta !== flags.meta) return false;
  // Shift behaviour: enforce strict equality so the JSDoc contract
  // ("must match exactly") is the truth. A chord without `shift+` will
  // not fire while shift is held, and a `ctrl+shift+x` chord will not
  // fire on a plain `ctrl+x` keystroke. Named keys (`tab`, `escape`,
  // arrows…) follow the same rule. Callers that genuinely want a
  // shift-tolerant binding should declare both forms.
  if (parsed.shift !== flags.shift) return false;
  const namedFlag = NAMED_KEYS[parsed.key];
  if (namedFlag !== undefined) {
    return flags[namedFlag];
  }
  // Plain character key. Ink hands us the raw character in `input`.
  // For Ctrl+letter combinations many terminals deliver the ASCII
  // control byte (`Ctrl+A` → `\x01`, …, `Ctrl+Z` → `\x1A`) while the
  // `key.ctrl` flag is also true; some Ink configurations leave
  // `input` empty instead. We accept three shapes for `ctrl+<letter>`:
  //
  //   1. `input` matches the chord key directly (e.g. `"p"` for
  //      `ctrl+p`) — Ink's behaviour on most platforms.
  //   2. `input` is the ASCII control byte for the chord key
  //      (e.g. `"\x10"` for `ctrl+p`) — what xterm-style terminals
  //      put on the wire. We compute the expected byte here so a
  //      `ctrl+p` chord no longer matches a `ctrl+,` keystroke.
  //   3. `input` is empty AND the chord key is a single character —
  //      ambiguous, but unavoidable when Ink hides both the typed
  //      character and the control byte. In that case any single-letter
  //      `ctrl+<x>` chord will match; callers that need precision should
  //      configure terminals that surface either the letter or the
  //      control byte. This branch is documented in the test suite.
  if (parsed.ctrl) {
    if (input.length === 0) {
      return parsed.key.length === 1;
    }
    if (parsed.key.length === 1) {
      const expected = parsed.key.toLowerCase();
      const code = expected.charCodeAt(0);
      // Ctrl+a..z → 1..26; only meaningful for ASCII letters.
      if (code >= 97 && code <= 122) {
        const controlByte = String.fromCharCode(code - 96);
        if (input === controlByte) return true;
      }
    }
  }
  return input.toLowerCase() === parsed.key.toLowerCase();
}
