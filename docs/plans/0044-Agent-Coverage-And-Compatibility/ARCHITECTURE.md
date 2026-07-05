# Architecture — Plan 0044 (deltas)

> The concrete deltas. This plan touches
> `crates/makina-acp/src/protocol.rs`,
> `crates/makina-acp/src/transport.rs`,
> `crates/makina-core/src/preflight.rs`,
> `crates/makina-core/src/config.rs`, `crates/makina/src/event.rs`,
> `.makina/config.toml`, and `README.md`.
> Line numbers are hints; locate by symbol.

## 0001 — JSON-RPC Id Correctness

Today `IncomingMessage.id` is `pub id: Option<u64>` with `#[serde(default)]`
(`protocol.rs:80-81`), and `classify()` (`protocol.rs:98-109`) matches
`(self.id, self.method)` to route lines. Our own outgoing request ids are
always numeric (`TransportSender::alloc_id -> u64`, `transport.rs:157-162`;
`pending: Mutex<HashMap<u64, …>>`, `transport.rs:78`). But an inbound
agent→client request such as `session/request_permission` may carry a JSON-RPC
*string* id; that line fails to deserialize into `IncomingMessage`, so the
reader's lenient `Err(_)` arm skips it (`transport.rs:432-437`), no response is
written, and the untimed prompt turn hangs. `route_message` extracts
`let req_id = message.id.expect(…)` (`transport.rs:492`) and echoes it via
`send_response`/`send_error_response` (`transport.rs:254-264`), both typed
`id: u64`.

**Edits:**

**Add a `RequestId` type and make the inbound id flexible (`protocol.rs`).**

```rust
/// A JSON-RPC 2.0 request/response id: a string OR a number (per the spec).
/// Untagged so `7` parses as `Number` and `"perm-1"` as `String`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RequestId { Number(u64), String(String) }

pub struct IncomingMessage {
    // …
    #[serde(default)]
    pub id: Option<RequestId>, // was Option<u64>
    // …
}
```

**Keep response correlation numeric, route requests verbatim (`classify`).**

```rust
pub fn classify(&self) -> IncomingKind {
    match (&self.id, self.method.as_deref()) {
        // Our correlated responses always carry the numeric id we issued.
        (Some(RequestId::Number(id)), None) => IncomingKind::Response { id: *id },
        (None, Some(_)) => IncomingKind::Notification,
        (Some(_), Some(_)) => IncomingKind::Request, // string OR number id
        _ => IncomingKind::Malformed,
    }
}
```

**Echo the inbound id verbatim on the way out (`protocol.rs` +
`transport.rs`).** `OutgoingResponse.id` and `OutgoingErrorResponse.id` become
`RequestId`; `send_response`/`send_error_response` take `id: RequestId`;
`route_message` passes `message.id.expect(...)` (now a `RequestId`) straight
through.

**Properties that make this safe:**

- The `pending` map and `alloc_id` stay `u64`, so response correlation is
  unchanged.
- Only inbound Requests (which we never correlate, only answer) can now carry
  a string id.
- An explicit `null` id still deserializes to `None` (serde `Option`),
  classifying as before.
- Every existing numeric-id test keeps its outcome after mechanically wrapping
  literals in `RequestId::Number(..)`.

## 0002 — Expanded KNOWN_AGENTS Launch Profiles

Today `KNOWN_AGENTS` (`preflight.rs:24-40`) has exactly three entries: `gemini`
with `args: &["--acp", "--yolo"]`, `claude-code-acp` with `args: &["--acp"]`,
and `grok` with `args: &["--acp"]`. `detect_backend_in_path`
(`preflight.rs:166-188`) returns the first entry whose `command` resolves on
`$PATH`, iterating agents outer / path dirs inner, so array order is detection
priority. The in-file test `detect_backend_in_path_finds_first_supported_agent`
(`preflight.rs:447-460`) asserts gemini's args are `["--acp", "--yolo"]`, and
the module doc (`preflight.rs:21-23`) says `gemini --acp --yolo`.

**Edits:**

**Rewrite the registry (`preflight.rs`).**

```rust
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    // Proven first-tier (gemini is the e2e-proven path; --yolo dropped, WorktreePolicy handles permissions).
    KnownAgent { name: "gemini",          command: "gemini",          args: &["--acp"] },
    KnownAgent { name: "claude-code-acp", command: "claude-code-acp", args: &[] }, // Zed adapter: no launch flag
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

**Fix the in-file assertions.** In
`detect_backend_in_path_finds_first_supported_agent` (`preflight.rs:459`)
change the expected args to `vec!["--acp".to_string()]`; update the module doc
(`preflight.rs:21-23`) to `gemini --acp`.

**Properties that make this safe:**

- The change is data-only (the pure detector is untouched and still spawns
  nothing, honoring plan 0015's discipline).
- The proven three agents stay first in priority order.
- `agent` is last so a stray `agent` binary is only ever matched when no other
  supported CLI exists.
- The registry remains the single source of truth every other surface reads.

## 0003 — Registry-Derived Surface Sync

Today three surfaces enumerate the supported CLIs. The empty-backend
validation reason (`config.rs:904-917`) builds its list dynamically via
`KNOWN_AGENTS.iter().map(|a| a.command)`, but the only test asserting its
content (`empty_backend_error_lists_supported_agents_and_file`) checks just
`"gemini"`. The Doctor scaffold's `build_global_template` (`event.rs:997-1024`)
lists all `KNOWN_AGENTS` commands dynamically in its no-detection branch
(asserted by `build_global_template_none_leaves_backend_commented`,
`event.rs:4800-4825`) but hardcodes a `# args = ["--acp", "--yolo"]` example
line (`event.rs:1018`). The shipped `.makina/config.toml` comment hardcodes
`# Makina auto-detects a supported agent CLI on $PATH (gemini,
claude-code-acp, grok)` (`.makina/config.toml:13`).

**Edits:**

**Guard the validation message (`config.rs`, test-only).** Add a regression
test iterating `KNOWN_AGENTS` and asserting each `command` appears in the
reason, so a future registry addition that forgets a surface fails a gate.

**Fix the scaffold example (`event.rs`).** Change the commented placeholder at
`event.rs:1018` from `# args = ["--acp", "--yolo"]` to `# args = ["--acp"]`
(drop the dead flag); the dynamic KNOWN_AGENTS listing in the same branch is
unchanged.

**Fix the shipped comment (`.makina/config.toml`).** Rewrite line 13's
parenthetical so it does not hardcode a stale three-name list — e.g.
`# Makina auto-detects a supported agent CLI on $PATH (gemini,
claude-code-acp, grok, and others — see the README compatibility matrix)`.

**Properties that make this safe:**

- The validation message and the scaffold no-detection branch already source
  their lists from `KNOWN_AGENTS`, so they update automatically.
- The new test converts that implicit coupling into an enforced one.
- The `.makina/config.toml` edit is comments-only (no keys change), so parsing
  and precedence are unaffected.

## 0004 — README Compatibility Matrix

Today the README names only gemini-cli as the required agent
(`README.md:43-45`) and shows one `[backend]` with `command = "gemini"` /
`args = ["--acp"]` (`README.md:58-68`); nothing enumerates the other supported
agents or how to launch each.

**Edits:**

**Add a compatibility matrix (`README.md`).** Under Requirements/Configure,
add a table whose rows mirror `KNOWN_AGENTS` exactly:

```markdown
| Agent | Install | Launch (`command` + `args`) | Sign-in |
|-------|---------|-----------------------------|---------|
| Gemini CLI | google-gemini/gemini-cli | `gemini --acp` | run `gemini` once |
| Claude Code (ACP) | @zed-industries/claude-code-acp | `claude-code-acp` | Claude sign-in |
| Grok | grok CLI | `grok --acp` | grok sign-in |
| GitHub Copilot CLI | `copilot` | `copilot --acp` | `gh`/Copilot auth |
| opencode | opencode CLI | `opencode acp` | opencode auth |
| Codex (codex-acp) | zed codex-acp adapter | `codex-acp` | Codex/OpenAI auth |
| Qwen Code | `qwen` | `qwen --experimental-acp` | qwen auth |
| Goose | block/goose | `goose acp` | goose config |
| Kilo | @kilocode/cli | `kilo acp` | kilo auth |
| Cursor CLI | `agent` | `agent acp` | cursor auth |
```

Follow with a per-agent `[backend]` snippet block and a note that Makina
auto-detects the first installed agent (registry order) so no config is needed
when one is present.

**Properties that make this safe:**

- Documentation-only (no compiled surface).
- The launch column is copied verbatim from the registry, so a reviewer can
  diff it against `preflight.rs`.
- The matrix makes the supported set visible, satisfying the plan's "visible"
  half.

## Test strategy

- **0001 (id correctness).** In `crates/makina-acp/src/protocol.rs`:
  `incoming_string_id_request_classifies_as_request` decodes
  `{"id":"perm-1","method":"session/request_permission"}` and asserts
  `id == Some(RequestId::String("perm-1"))` and `classify() == Request` (red
  before: the line failed to decode); `incoming_message_classification` stays
  green for numeric ids; `outgoing_response_envelope_serializes_with_id_and_result`
  wraps `RequestId::Number(42)` and still asserts `v["id"] == 42`. In
  `crates/makina-acp/src/transport.rs`:
  `permission_request_with_string_id_is_answered` scripts an in-memory peer
  sending a `session/request_permission` with `id = "perm-str-1"` and asserts
  the client writes a response echoing `resp["id"] == "perm-str-1"` with the
  allow-once outcome (red before: no response was written and the turn hung).
- **0002 (registry).** In `preflight.rs`:
  `known_agents_registry_has_expected_launch_profiles` asserts gemini has no
  `--yolo`, claude-code-acp has empty args, all seven added agents are present,
  and `agent`/cursor is last; the updated
  `detect_backend_in_path_finds_first_supported_agent` asserts gemini resolves
  with `["--acp"]`.
- **0003 (surface sync).** In `config.rs`:
  `empty_backend_error_names_every_known_agent` iterates `KNOWN_AGENTS` and
  asserts each command appears in the validation reason; in `event.rs`,
  `build_global_template_none_leaves_backend_commented` continues to assert
  every registry command is listed in the scaffold's no-detection template.
- **0004 (README).** Documentation-only; verified by review that the Launch
  column matches `preflight.rs`.
- All tasks keep `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo fmt --check` green.

## Interaction with prior work

- **0040 — ACP Protocol Hardening & Observability.** This plan continues
  0040's inbound-request handling: the transport already answers
  `session/request_permission` and replies method-not-found to unknown inbound
  requests so a peer never hangs (`transport.rs:587-604`); WS0001 removes the
  remaining gap where a *string*-id inbound request never reached that handler.
- **0043 — First-Run Config UX / Auto-Detected Defaults.** This plan extends
  0043's `KNOWN_AGENTS` registry and its registry-derived surfaces
  (`detect_backend_in_path`, `apply_detected_backend`, the Doctor `w`
  scaffold, the enriched validation message), keeping them the single source
  of truth as the supported set grows.
- **0024 — Permission Policy & Sandbox Teeth (WorktreePolicy).** Dropping
  gemini's `--yolo` is safe precisely because `WorktreePolicy` now
  auto-answers permission requests inside the assigned worktree
  (crates/makina/tests/e2e.rs:51-67), so the bypass flag is dead.
- **0015 — Idle-Hang Detection.** Detection remains spawn-free (pure PATH
  stat), honoring 0015's finding that spawning an unauthenticated agent can
  hang; no added agent is executed to verify it.
