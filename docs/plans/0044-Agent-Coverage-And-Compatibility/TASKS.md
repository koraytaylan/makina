# XAgent Plan 0044 — Agent Coverage & Compatibility — Verified ACP Agent Support

This plan makes Makina's ACP agent coverage both correct and visible: it replaces `IncomingMessage.id: Option<u64>` (crates/makina-acp/src/protocol.rs:81) with a flexible `RequestId` (string OR number) and threads it through `classify()`, the outgoing response envelopes, and the transport's `route_message`/`send_response` so an agent→client `session/request_permission` carrying a JSON-RPC *string* id is routed as a Request and answered instead of being silently dropped (which today hangs the untimed prompt turn); it rewrites the `KNOWN_AGENTS` registry (crates/makina-core/src/preflight.rs:24-40) to drop gemini's obsolete `--yolo`, launch `claude-code-acp` with no args (the Zed adapter defines no `--acp`), and add launch profiles for GitHub Copilot CLI, opencode, Codex (codex-acp), Qwen Code, Goose, Kilo, and Cursor; it keeps the three registry-derived surfaces — the empty-backend validation message (config.rs:904-917), the Doctor `w` scaffold template (event.rs:997-1024), and the shipped `.makina/config.toml` comment (line 13) — in lock-step with the expanded registry with tests asserting every entry is named; and it replaces the README's gemini-only story with a supported-agent compatibility matrix and per-agent `[backend]` launch snippets — all with the three quality gates green.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — JSON-RPC Id Correctness

### flexible-jsonrpc-request-id — Accept String or Numeric JSON-RPC Request Ids End-to-End

`IncomingMessage.id` is `pub id: Option<u64>` (`crates/makina-acp/src/protocol.rs:81`) and `classify()` (`protocol.rs:98-109`) routes on it. Our own outgoing request ids are always numeric (`alloc_id -> u64`, `pending: HashMap<u64, …>`, `transport.rs:78,157-162`), but an agent→client request such as `session/request_permission` may carry a JSON-RPC *string* id. Today that line fails to deserialize into `IncomingMessage`, so the reader's lenient `Err(_)` arm skips it (`transport.rs:432-437`), no response is written, and the untimed `session/prompt` turn hangs forever. The protocol and transport id types are tightly coupled — `route_message` extracts `let req_id = message.id.expect(…)` (`transport.rs:492`) and echoes it via `send_response`/`send_error_response` (`transport.rs:254-264`), both typed `id: u64` — so the id-type change must land as one compiling unit.

**Steps:**

1. In `crates/makina-acp/src/protocol.rs`, add the flexible id type just above `IncomingMessage` (near `protocol.rs:72`):

   ```rust
   /// A JSON-RPC 2.0 request/response id: a string OR a number (per the spec).
   /// Untagged so `7` parses as `Number` and `"perm-1"` as `String`. An explicit
   /// JSON `null` id still deserializes to `Option::None` (serde `Option`).
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
   #[serde(untagged)]
   pub enum RequestId {
       /// Numeric id (the form Makina issues for its own requests).
       Number(u64),
       /// String id (some agents use these for server→client requests).
       String(String),
   }
   ```

2. Change the `IncomingMessage.id` field (`protocol.rs:80-81`) from `pub id: Option<u64>` to `pub id: Option<RequestId>` (keep the `#[serde(default)]`).

3. Rewrite `classify()` (`protocol.rs:98-109`) to keep numeric correlation while routing string-id requests:

   ```rust
   pub fn classify(&self) -> IncomingKind {
       match (&self.id, self.method.as_deref()) {
           // Our correlated responses always carry the numeric id we issued.
           (Some(RequestId::Number(id)), None) => IncomingKind::Response { id: *id },
           // A notification has a method and no id.
           (None, Some(_)) => IncomingKind::Notification,
           // A server→client request has an id (string OR number) and a method.
           (Some(_), Some(_)) => IncomingKind::Request,
           // Anything else (incl. a bare string-id "response" we never issued).
           _ => IncomingKind::Malformed,
       }
   }
   ```

   Leave `IncomingKind::Response { id: u64 }` (`protocol.rs:114-119`) unchanged.

4. Change the outgoing envelopes so the inbound id can be echoed verbatim: in `protocol.rs`, change `OutgoingResponse.id` (`protocol.rs:211`) and `OutgoingErrorResponse.id` (`protocol.rs:234`) from `pub id: u64` to `pub id: RequestId`, and change both `::new(id: u64, …)` constructors' `id` params to `id: RequestId`.

5. In `crates/makina-acp/src/transport.rs`, change `send_response` (`transport.rs:254`) and `send_error_response` (`transport.rs:264`) signatures from `id: u64` to `id: crate::protocol::RequestId`. `route_message` (`transport.rs:492`) now yields `req_id: RequestId` from `message.id.expect(...)`; it already flows into `send_response`/`send_error_response`, so no further body change is needed. Import `RequestId` alongside the other `protocol` imports (`transport.rs:40-41`).

6. Update the numeric-id tests in place so they still compile and pass: in `protocol.rs`, `incoming_message_classification` (`protocol.rs:934-955`) already feeds numeric ids — it stays green; `outgoing_response_envelope_serializes_with_id_and_result` (`protocol.rs:1061`) must change `OutgoingResponse::new(42, …)` to `OutgoingResponse::new(RequestId::Number(42), …)` and still assert `v["id"] == 42`.

7. Add a protocol decode test proving the bug is fixed:

   ```rust
   #[test]
   fn incoming_string_id_request_classifies_as_request() {
       let msg: IncomingMessage = serde_json::from_value(serde_json::json!({
           "jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission"
       })).expect("a string-id request must decode");
       assert_eq!(msg.id, Some(RequestId::String("perm-1".to_string())));
       assert_eq!(msg.classify(), IncomingKind::Request);
   }
   ```

   (Before this task the `from_value` fails, so the line was dropped upstream; after it, it classifies as `Request`.)

8. Add a transport round-trip regression test: copy `permission_request_mid_turn_is_answered_and_audited` (`transport.rs:791`) to a new `#[tokio::test]` named `permission_request_with_string_id_is_answered`, and change exactly two things — set `let perm_id = "perm-str-1";` (a `&str`, replacing `let perm_id = 42u64;` at `transport.rs:844`; `"id": perm_id` in the request json still works) and change the response-id assertion `assert_eq!(resp["id"], perm_id);` (`transport.rs:867`) to `assert_eq!(resp["id"], "perm-str-1");`. The rest (policy, allow-once selection, turn completion) is unchanged.

9. Run the full gate commands.

- **Depends on:** —
- **Done when:** `protocol::RequestId` exists; `IncomingMessage.id` is `Option<RequestId>`; `classify()` returns `Request` for a string-id request and still `Response{id}` for a numeric id; `OutgoingResponse`/`OutgoingErrorResponse`/`send_response`/`send_error_response` carry `RequestId`; `incoming_string_id_request_classifies_as_request` and `permission_request_with_string_id_is_answered` pass (both would fail red before this task — the string-id request was dropped and no response written), and the pre-existing numeric-id tests stay green. cargo test/clippy/fmt green.

---

## 0002 — Expanded KNOWN_AGENTS Launch Profiles

### expand-known-agents — Expand KNOWN_AGENTS With Verified ACP Launch Profiles

`KNOWN_AGENTS` (`crates/makina-core/src/preflight.rs:24-40`) lists only `gemini` (`args: &["--acp", "--yolo"]`), `claude-code-acp` (`args: &["--acp"]`), and `grok`. The `--yolo` flag is dead now that `WorktreePolicy` answers permission requests (`crates/makina/tests/e2e.rs:51-67`: "no longer needed or used"), the `claude-code-acp` adapter takes no launch flag (`crates/makina-acp/src/lib.rs:4-5`), and seven other ACP-capable agents cannot be detected at all. `detect_backend_in_path` returns the first registry entry found on `$PATH` in array order, so order is detection priority and Cursor's generic `agent` binary must go last. This task is data-only; the pure spawn-free detector is untouched.

**Steps:**

1. In `crates/makina-core/src/preflight.rs`, replace the `KNOWN_AGENTS` array (`preflight.rs:24-40`) with:

   ```rust
   pub const KNOWN_AGENTS: &[KnownAgent] = &[
       // Proven first-tier (gemini is the e2e-proven path, docs/trial/e2e-run.md).
       // --yolo dropped: WorktreePolicy now answers session/request_permission.
       KnownAgent { name: "gemini",          command: "gemini",          args: &["--acp"] },
       // The Zed claude-code-acp adapter speaks ACP on stdio with no launch flag.
       KnownAgent { name: "claude-code-acp", command: "claude-code-acp", args: &[] },
       KnownAgent { name: "grok",            command: "grok",            args: &["--acp"] },
       // Additional ACP-capable agents (documented invocations; never spawned to verify).
       KnownAgent { name: "copilot",         command: "copilot",         args: &["--acp"] },
       KnownAgent { name: "opencode",        command: "opencode",        args: &["acp"] },
       KnownAgent { name: "codex-acp",       command: "codex-acp",       args: &[] },
       KnownAgent { name: "qwen",            command: "qwen",            args: &["--experimental-acp"] },
       KnownAgent { name: "goose",           command: "goose",           args: &["acp"] },
       KnownAgent { name: "kilo",            command: "kilo",            args: &["acp"] },
       // Cursor's CLI binary is the generic name `agent`; kept LAST to minimize
       // false-positive detection against an unrelated `agent` on PATH.
       KnownAgent { name: "cursor",          command: "agent",           args: &["acp"] },
   ];
   ```

2. Update the module doc comment (`preflight.rs:21-23`) so it reads `gemini --acp` (drop `--yolo`) and no longer implies claude passes `--acp`.

3. Fix the existing in-file test `detect_backend_in_path_finds_first_supported_agent` (`preflight.rs:447-460`): change the args assertion from `vec!["--acp".to_string(), "--yolo".to_string()]` to `vec!["--acp".to_string()]`.

4. Add a registry-shape regression test in `preflight.rs`'s `#[cfg(test)] mod tests`:

   ```rust
   #[test]
   fn known_agents_registry_has_expected_launch_profiles() {
       use std::collections::HashMap;
       let by_name: HashMap<&str, &KnownAgent> =
           KNOWN_AGENTS.iter().map(|a| (a.name, a)).collect();
       // gemini drops the obsolete --yolo bypass.
       let gemini = by_name.get("gemini").expect("gemini present");
       assert_eq!(gemini.args, &["--acp"]);
       assert!(!gemini.args.contains(&"--yolo"), "gemini must not pass --yolo");
       // The Zed claude-code-acp adapter launches with no args.
       let claude = by_name.get("claude-code-acp").expect("claude-code-acp present");
       assert!(claude.args.is_empty(), "claude-code-acp must launch with no args");
       // The seven added agents are all registered.
       for name in ["copilot", "opencode", "codex-acp", "qwen", "goose", "kilo", "cursor"] {
           assert!(by_name.contains_key(name), "missing agent: {name}");
       }
       // Cursor's generic `agent` binary must be LAST to reduce false positives.
       assert_eq!(KNOWN_AGENTS.last().unwrap().command, "agent");
   }
   ```

5. Run the full gate commands.

- **Depends on:** —
- **Done when:** `KNOWN_AGENTS` contains all ten entries with the exact commands/args above (gemini has no `--yolo`, claude-code-acp has empty args, `agent`/cursor is last); `known_agents_registry_has_expected_launch_profiles` passes and the updated `detect_backend_in_path_finds_first_supported_agent` (now asserting `["--acp"]`) passes. cargo test/clippy/fmt green.

---

## 0003 — Registry-Derived Surface Sync

### sync-scaffold-and-validation — Sync Doctor Scaffold and Validation Message to the Expanded Registry

Two registry-derived surfaces need synchronizing after the registry grows. The Doctor scaffold's no-detection template hardcodes a `# args = ["--acp", "--yolo"]` example (`crates/makina/src/event.rs:1018`) even though `--yolo` is now dead, and the empty-backend validation reason (`crates/makina-core/src/config.rs:904-917`) already lists `KNOWN_AGENTS` dynamically but is only guarded by a test that checks for `"gemini"`. Both the scaffold's no-detection branch (`event.rs:1009-1023`) and the validation message enumerate the registry dynamically, so they update automatically — this task fixes the one stale example line and adds the missing enforced-coupling test so a future registry addition that forgets a surface fails a gate.

**Steps:**

1. In `crates/makina/src/event.rs`, in `build_global_template`'s no-detection branch (`event.rs:1018`), change the commented placeholder `# args = ["--acp", "--yolo"]` to `# args = ["--acp"]`. Leave the dynamic `KNOWN_AGENTS` listing (`event.rs:1009-1013`) and the detected branch unchanged; `build_global_template_none_leaves_backend_commented` (`event.rs:4800-4825`) already asserts every registry command is named and stays green.

2. In `crates/makina-core/src/config.rs`'s `#[cfg(test)] mod tests`, add a regression test asserting the empty-backend reason names every supported CLI (mirroring the existing `empty_backend_error_lists_supported_agents_and_file` call shape):

   ```rust
   #[test]
   fn empty_backend_error_names_every_known_agent() {
       let no_global = std::path::Path::new("/tmp/__makina_allagents_g.toml");
       let no_project = std::path::Path::new("/tmp/__makina_allagents_p.toml");
       match Config::load_with_labels(Some(no_global), None, Some(no_project), None, Some("")) {
           Err(ConfigError::Validation { reason }) => {
               for agent in crate::preflight::KNOWN_AGENTS {
                   assert!(
                       reason.contains(agent.command),
                       "validation message must name supported CLI {:?}; got: {reason}",
                       agent.command,
                   );
               }
           }
           other => panic!("expected Validation error, got: {other:?}"),
       }
   }
   ```

3. Run the full gate commands.

- **Depends on:** expand-known-agents
- **Done when:** The Doctor scaffold's no-detection example no longer shows `--yolo`; `empty_backend_error_names_every_known_agent` passes (asserting the validation reason contains all ten `KNOWN_AGENTS` commands, incl. `copilot`/`opencode`/`codex-acp`/`qwen`/`goose`/`kilo`/`agent`), and `build_global_template_none_leaves_backend_commented` stays green. cargo test/clippy/fmt green.

---

### sync-shipped-config-comment — De-Hardcode the Shipped .makina/config.toml Supported-Agent Comment

The shipped project config comment hardcodes the three-name list `# Makina auto-detects a supported agent CLI on $PATH (gemini, claude-code-acp, grok)` (`.makina/config.toml:13`). After the registry expands to ten agents this comment is stale and will silently drift on every future addition. This is a comments-only change to the committed project layer; no keys change and the file must remain valid TOML (the committed project config stays backend-free, per plan 0043's locked decision).

**Steps:**

1. In `.makina/config.toml`, rewrite the parenthetical on line 13 so it no longer pins a stale three-name list — e.g.: `# Makina auto-detects a supported agent CLI on $PATH (gemini, claude-code-acp, grok, and others — see the README compatibility matrix) and uses it as the default backend`. Keep the rest of the comment block (lines 14-18: override guidance and the Doctor `w` pointer) intact.

2. Do NOT add any `[backend]` or `[[providers]]` table — the backend is machine-specific and belongs to `~/.makina/config.toml`. Leave `base_branch`, `concurrency`, `[caps]`, and `[[gates]]` (`.makina/config.toml:23-61`) unchanged.

3. Run the full gate commands (the config crate parses this file as a fixture during `cargo test`).

- **Depends on:** expand-known-agents
- **Done when:** Documentation-only (a comment in a shipped config; no code path changes). The `.makina/config.toml:13` comment no longer hardcodes the stale `(gemini, claude-code-acp, grok)` triple, no backend/provider table is added, and the file remains valid TOML. cargo test/clippy/fmt green.

---

## 0004 — README Compatibility Matrix

### readme-compat-matrix — Publish the Supported-Agent Compatibility Matrix in the README

The README names only gemini-cli as the required agent (`README.md:43-45`) and shows a single `command = "gemini"` `[backend]` (`README.md:58-68`), so a reader cannot see which agents Makina supports or how to launch each — the coverage is invisible. This task replaces the gemini-only story with a compatibility matrix and per-agent `[backend]` snippets whose launch commands/args are copied verbatim from `KNOWN_AGENTS` so docs and code cannot drift. Documentation-only.

**Steps:**

1. In `README.md`, replace the single-agent Requirements bullet (`README.md:43-45`) with a pointer to a new "Supported agents" compatibility matrix, and add that matrix (one row per `KNOWN_AGENTS` entry, in registry/priority order) with columns Agent, Install source, Launch (`command` + `args`), and Sign-in note. The Launch column MUST match `crates/makina-core/src/preflight.rs` exactly: `gemini --acp`, `claude-code-acp` (no args), `grok --acp`, `copilot --acp`, `opencode acp`, `codex-acp` (no args), `qwen --experimental-acp`, `goose acp`, `kilo acp`, `agent acp` (Cursor).

2. Rewrite the Configure section's example (`README.md:58-68`) so it (a) states Makina auto-detects the first installed agent in registry order (no global config needed when one is present), and (b) shows per-agent `[backend]` snippets, e.g. `command = "claude-code-acp"` / `args = []` and `command = "qwen"` / `args = ["--experimental-acp"]`, alongside the existing gemini example. Keep the two-layer (global/project) explanation.

3. Sanity-check the doc: every agent name/command/args in the README table matches the `KNOWN_AGENTS` array from `expand-known-agents` (a reviewer diffs the Launch column against `preflight.rs`).

4. Run the full gate commands (README is not compiled, but the gates must remain green).

- **Depends on:** expand-known-agents
- **Done when:** Documentation-only (no runtime surface, so the red-green gate is exempt). `README.md` contains a supported-agent compatibility matrix with one row per `KNOWN_AGENTS` entry whose Launch column is byte-identical to `preflight.rs`, plus per-agent `[backend]` snippets, and the gemini-only Requirements/Configure text is replaced. cargo test/clippy/fmt green.

---

**End of plan 0044 TASKS.** When every "Done when" bullet is green, Makina's
ACP agent support is visible and verified: string-id agent→client requests are
decoded, routed as Requests, and answered verbatim (no hung prompt turns), the
`KNOWN_AGENTS` registry lists ten correctly-launched ACP agents (gemini without
`--yolo`, claude-code-acp with no args, Cursor's generic `agent` binary last),
every registry-derived surface — the empty-backend validation message, the
Doctor `w` scaffold, and the shipped `.makina/config.toml` comment — stays in
lock-step with the registry under enforced-coupling tests, and the README
publishes a supported-agent compatibility matrix whose launch column mirrors
`preflight.rs` byte-for-byte — all with the gate commands green.
