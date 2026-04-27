/**
 * Wave 0 placeholder so the coverage gate has a measurable production file
 * to report against. Wave 1 deletes this file and replaces `src/` with the
 * real foundational modules (`types.ts`, `constants.ts`, `config/`, `ipc/`).
 */

/**
 * Return a friendly greeting line.
 *
 * @param name The recipient. Trimmed of surrounding whitespace; an empty
 *   string after trimming yields the generic greeting `"Hello!"`.
 * @returns A short greeting string.
 *
 * @example
 * ```ts
 * greet("world"); // "Hello, world!"
 * greet("");      // "Hello!"
 * ```
 */
export function greet(name: string): string {
  const trimmed = name.trim();
  if (trimmed.length === 0) {
    return "Hello!";
  }
  return `Hello, ${trimmed}!`;
}
