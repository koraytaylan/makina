/**
 * Public sub-export `@makina/core/test-helpers`.
 *
 * In-memory doubles that implement the same interfaces as the production
 * adapters. Both `@makina/core`'s own tests and downstream consumers' tests
 * use them to drive the supervisor without touching real GitHub, real git
 * worktrees, or real subprocesses.
 */

export * from "./in_memory_daemon_client.ts";
export * from "./in_memory_github_auth.ts";
export * from "./in_memory_github_client.ts";
export * from "./mock_agent_runner.ts";
