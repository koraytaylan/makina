# Scope — Plan 0044

> Make Makina's ACP agent support visible and verified by fixing the JSON-RPC string-id bug that silently drops agent→client permission requests, expanding `KNOWN_AGENTS` with correct launch profiles for the ACP-capable agents (fixing claude-code-acp's undocumented `--acp` and dropping gemini's obsolete `--yolo`), keeping every registry-derived surface in sync, and publishing a README compatibility matrix.

## Why this plan

**1. The `KNOWN_AGENTS` registry advertises only three of the ACP-capable agents.** `KNOWN_AGENTS` (`preflight.rs:24-40`) lists only `gemini`, `claude-code-acp`, and `grok`, so auto-detection, the Doctor scaffold, and every registry-derived surface can never discover GitHub Copilot CLI, opencode, Codex, Qwen Code, Goose, Kilo, or Cursor even when one is the only agent installed — the supported set is invisible and unverified.

**2. The `claude-code-acp` entry passes an undocumented `--acp` flag the Zed adapter never defines.** The `claude-code-acp` `KnownAgent` (`preflight.rs:30-34`) sets `args: &["--acp"]`, but that binary is the Zed-compatible ACP adapter (`crates/makina-acp/src/lib.rs:4-5`) which speaks ACP on stdio with no launch flag, so a detected/scaffolded `claude-code-acp --acp` can fail to start.

**3. The `gemini` default still carries a `--yolo` holdover.** The `gemini` `KnownAgent` (`preflight.rs:25-29`) sets `args: &["--acp", "--yolo"]`, but the permission-bypass flag is dead now that a `WorktreePolicy` answers `session/request_permission` transparently — the e2e harness records that `--yolo` "is no longer needed or used" (`crates/makina/tests/e2e.rs:51-67`).

**4. `IncomingMessage.id` is `Option<u64>`, so an agent→client request with a JSON-RPC *string* id is silently dropped and hangs the turn.** `IncomingMessage.id` is `pub id: Option<u64>` (`protocol.rs:81`) and `classify()` (`protocol.rs:98-109`) matches on it; a line like `{"id":"perm-1","method":"session/request_permission"}` fails to deserialize entirely, so the reader's lenient `Err(_)` arm (`transport.rs:432-437`) skips it, no response is written, and the untimed `session/prompt` turn waits forever for a permission decision that never comes.

**5. The registry-derived surfaces hardcode or must re-list the supported set and will drift as the registry grows.** The empty-backend validation reason (`config.rs:904-917`), the Doctor `w` scaffold template (`event.rs:997-1024`), and the shipped project-config comment `# … on $PATH (gemini, claude-code-acp, grok)` (`.makina/config.toml:13`) all enumerate the supported CLIs; the last hardcodes the three names, and none is guarded by a test that every `KNOWN_AGENTS` entry is named.

**6. The README tells a gemini-only story.** The requirements bullet names only gemini-cli (`README.md:43-45`) and the Configure section shows a single `command = "gemini"` `[backend]` (`README.md:58-68`), so a reader cannot see which agents Makina supports or how to launch each — the coverage is undocumented.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — JSON-RPC Id Correctness.** Replace `IncomingMessage.id: Option<u64>` (`protocol.rs:81`) with a flexible `RequestId` (JSON-RPC string OR number), update `classify()` (`protocol.rs:98-109`) and the outgoing `OutgoingResponse`/`OutgoingErrorResponse` id fields, and thread `RequestId` through the transport's `route_message` and `send_response`/`send_error_response` (`transport.rs:254-264,448-611`) so an agent→client `session/request_permission` carrying a string id is routed as a Request and answered verbatim rather than silently dropped; the response-correlation `pending` map stays keyed by our own numeric `u64` ids. Add a protocol decode test and a transport string-id round-trip test.
- **0002 — Expanded KNOWN_AGENTS Launch Profiles.** Rewrite the `KNOWN_AGENTS` registry (`preflight.rs:24-40`) to drop gemini's obsolete `--yolo` (leaving `gemini --acp`), launch `claude-code-acp` with **no** args (the Zed adapter defines no launch flag), keep `grok --acp`, and add launch profiles for GitHub Copilot CLI (`copilot --acp`), opencode (`opencode acp`), Codex (`codex-acp`), Qwen Code (`qwen --experimental-acp`), Goose (`goose acp`), Kilo (`kilo acp`), and Cursor (`agent acp`, kept LAST because `agent` is a generic binary name). Update the in-file module doc and the two in-file tests that assert the old gemini args, and add a registry-shape test.
- **0003 — Registry-Derived Surface Sync.** Keep the three registry-derived surfaces in lock-step with the expanded registry: drop `--yolo` from the Doctor scaffold's commented example (`event.rs:1018`) and add a `config.rs` test asserting the empty-backend validation reason (`config.rs:904-917`) names **every** `KNOWN_AGENTS` command; and update the shipped `.makina/config.toml` comment (line 13) that hardcodes `(gemini, claude-code-acp, grok)` so it no longer drifts. The `event.rs` no-detection template and the validation message already enumerate `KNOWN_AGENTS` dynamically, so this workstream mainly adds the missing regression guards and fixes the two hardcoded strings.
- **0004 — README Compatibility Matrix.** Replace the README's gemini-only requirements bullet (`README.md:43-45`) and single-`command` Configure snippet (`README.md:58-68`) with a supported-agent compatibility matrix (one row per `KNOWN_AGENTS` entry: agent, install source, launch `command`+`args`, and sign-in note) plus per-agent `[backend]` config snippets, keeping the launch commands/args byte-identical to the registry so docs and code cannot drift. Documentation-only.

## Origin -> workstream mapping

| Finding | Addressed by |
|---|---|
| 1 — `KNOWN_AGENTS` advertises only three agents (`preflight.rs:24-40`). | `0002` |
| 2 — `claude-code-acp` passes an undocumented `--acp` flag (`preflight.rs:30-34`; `lib.rs:4-5`). | `0002` |
| 3 — `gemini` default carries a dead `--yolo` holdover (`preflight.rs:25-29`; `e2e.rs:51-67`). | `0002` |
| 4 — `IncomingMessage.id` is `Option<u64>` so string-id requests are dropped and the turn hangs (`protocol.rs:81,98-109`; `transport.rs:432-437,491-492`). | `0001` |
| 5 — registry-derived surfaces hardcode/re-list the set and drift (`config.rs:904-917`; `event.rs:997-1024,1018`; `.makina/config.toml:13`). | `0003` |
| 6 — README tells a gemini-only story (`README.md:43-45,58-68`). | `0004` |

## Locked decisions

- **RequestId is an untagged Number(u64)/String(String); response correlation stays numeric.** The flexible id is `enum RequestId { Number(u64), String(String) }` with `#[serde(untagged)]`. Only inbound Requests (which Makina answers, never correlates) may carry a string id; the `pending` map and `alloc_id` remain `u64`, so `classify()` routes a numeric-id-no-method line to `Response{id: u64}` and any string-id "response" (which we never issue) to `Malformed`. Fractional and negative ids are out of scope (ACP agents use strings or non-negative integers); an explicit JSON `null` id deserializes to `None` as before. Revisit only if a future agent correlates responses by string id.
- **Detection stays spawn-free; "verified" means wire-compatible + single-source + tested, not per-binary execution.** No candidate CLI is executed to confirm it authenticates or speaks ACP (spawning an unauthenticated agent can hang — plan 0015). "Verified ACP agent support" is delivered by the string-id correctness fix (WS0001), the registry being the single source of truth every surface reads (WS0003 tests), and the documented launch profiles (WS0002/0004) — not by launching each binary. Revisit if a real-agent CI harness is added.
- **Registry order is detection priority; Cursor's generic `agent` binary is LAST.** `detect_backend_in_path` returns the first `KNOWN_AGENTS` entry found on `$PATH`, so the proven three (gemini, claude-code-acp, grok) stay first and the new agents follow. Cursor's CLI binary is the generic name `agent`, which risks matching an unrelated `agent` on PATH, so it is placed last (enforced by a test) to minimize false-positive detection.
- **claude-code-acp launches with no args; gemini drops --yolo.** The Zed `claude-code-acp` adapter (`crates/makina-acp/src/lib.rs:4-5`) speaks ACP on stdio and defines no launch flag, so its `args` become `&[]`. Gemini's `--yolo` permission bypass is dead now that `WorktreePolicy` answers `session/request_permission` transparently (`crates/makina/tests/e2e.rs:51-67`), so gemini launches as `gemini --acp`. Both are one-line registry edits reversible if an agent's documented invocation changes.
- **The committed .makina/config.toml stays backend-free; only its comments change.** Per plan 0043, the backend is machine-specific and belongs to `~/.makina/config.toml`; the shipped project config must never carry a `[backend]`/`[[providers]]` table. WS0003's `.makina/config.toml` edit is comments-only (de-hardcoding the supported-agent list), preserving parsing and precedence.

## Out of scope

- A user-editable agent registry manifest (e.g. a TOML file defining custom agents). Deferred registry-manifest work from the source brief; this plan keeps `KNOWN_AGENTS` as the single in-code source of truth and only expands it.
- PTY-fallback and direct-API (non-ACP) backends. Explicitly deferred in the plan rationale; Makina's model is external, already-authenticated ACP CLIs over stdio only.
- Spawning candidate agent CLIs to verify they authenticate or speak ACP. Spawning an unauthenticated agent can itself hang (plan 0015); detection stays pure filesystem/PATH resolution, matching the existing preflight discipline.
- Changing gemini's flag from --acp to --experimental-acp. The e2e harness and existing tests drive gemini with `--acp` (`crates/makina/tests/e2e.rs`); both aliases work, and switching is an unrelated, unverified change.
- A provider-picker UI or per-agent model/effort selection surface. Provider editing already exists (plans 0011/0017) and per-agent model selection is separate configurability; this plan only supplies correct launch profiles and visible documentation.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
