/**
 * Wave 0 sanity test. Exercises the `welcome` placeholder so the
 * `test:coverage` gate has something measurable to report against. Wave 1
 * removes both this test and `src/welcome.ts` and replaces them with the
 * real contract tests.
 */

import { assertEquals } from "@std/assert";
import { greet } from "../../src/welcome.ts";

Deno.test("welcome: named recipient", () => {
  assertEquals(greet("world"), "Hello, world!");
});

Deno.test("welcome: trims whitespace around name", () => {
  assertEquals(greet("  world  "), "Hello, world!");
});

Deno.test("welcome: empty name uses generic greeting", () => {
  assertEquals(greet(""), "Hello!");
});

Deno.test("welcome: whitespace-only name uses generic greeting", () => {
  assertEquals(greet("   "), "Hello!");
});
