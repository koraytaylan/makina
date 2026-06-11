# Makina Plan 0024 — Permission Policy & Sandbox Teeth

Make the permission policy actually decide something — admit tool calls by the
paths they touch, deny via the agent's offered reject option — make every
audit entry carry its real run/task identity and tool name, and give the
Docker gate sandbox real isolation flags (network off by default, opt-in
resource caps, a safe mount spec).

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0074 — Location-aware policy v2

### tool-call-path-view — Typed extraction of referenced paths

The flatten map already preserves `locations`/`content`/`_meta`
(`protocol.rs:629–631`), proven against a real `gemini --acp` capture
(`protocol.rs:938–1010`); nothing reads them yet.

**Steps:**

1. In `crates/makina-acp/src/protocol.rs`, add
   `enum PathExtraction { Absent, Paths(Vec<PathBuf>), Malformed }` and
   `ToolCall::referenced_paths(&self) -> PathExtraction` reading
   `extra["locations"]` (array of `{ "path": … }`) and `extra["content"]`
   entries carrying a `"path"` (diff blocks), per the captured shape
   (`protocol.rs:944–963`). Present-but-unparseable path data → `Malformed`.

2. Add tests:

   ```rust
   #[test]
   fn referenced_paths_extracts_locations_and_diff_paths() { /* captured payload → Paths([/tmp/fs-probe.txt, …]) */ }
   #[test]
   fn referenced_paths_absent_and_malformed_variants() { /* no path data → Absent; {"path": 42} → Malformed */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; the helper is pure (no fs access); the
  existing capture test (`protocol.rs:938`) is untouched; cargo
  test/clippy/fmt green.

### location-aware-worktree-policy — Admit by paths, not by a tautology

`WorktreePolicy::decide` compares `ctx.working_dir == self.worktree`
(`permission.rs:97`) — both derive from the same `command.working_dir`
(`client.rs:151–156`, `client.rs:767–771`, `transport.rs:457–462`), so the
deny branch (`permission.rs:123–131`) is unreachable in production.

**Steps:**

1. In `crates/makina-acp/src/permission.rs`, add `strict_paths: bool`
   (default `false`, builder method) to `WorktreePolicy` and re-implement
   `decide` over `ctx.tool_call.referenced_paths()`:
   `Paths` all lexically inside the worktree → allow (`allow_once` selection
   unchanged); any outside → deny naming the offending path; `Malformed` →
   deny; `Absent` → allow unless `strict_paths`. Lexical containment:
   normalize `.`/`..` components, join relative paths onto the worktree,
   then `starts_with`. Keep the `working_dir == worktree` check only as a
   mis-wiring guard (mismatch → deny + warn).

2. Rewrite the MVP-limitation doc (`permission.rs:77–80`) to state the new
   behaviour, the missing-vs-malformed rationale, and the symlink residual
   risk (deferred to `doc/plan/0001-Initial/FUTURE.md:52`).

3. Add tests:

   ```rust
   #[test]
   fn out_of_worktree_path_is_denied() { /* worktree /wt/task-a; write to /tmp/evil → deny; reason names the path */ }
   #[test]
   fn in_worktree_paths_are_allowed() { /* /wt/task-a/src/main.rs → allow_once */ }
   #[test]
   fn relative_and_dotdot_paths_normalize_before_check() { /* "src/main.rs" allows; "src/../../escape" denies */ }
   #[test]
   fn absent_paths_follow_strict_knob() { /* Absent → allow by default; deny with strict_paths */ }
   ```

- **Depends on:** tool-call-path-view
- **Done when:** all four tests pass; the existing permission tests
  (`permission.rs:179–283`) updated where their fixtures now carry paths;
  the transport round-trip tests (`transport.rs:745`,
  `tests/backend_trait.rs:482`) still pass (their `/tmp` fixture either gains
  in-worktree paths or asserts the new deny); cargo test/clippy/fmt green.

### deny-selects-reject-option — Refuse the call without cancelling the turn

Denial is currently expressed as `PermissionOutcome::Cancelled`
(`transport.rs:477–481`) even when `reject_once`/`reject_always` were offered
(`protocol.rs:603–610`).

**Steps:**

1. In `permission.rs`, on the deny path pick `reject_once`, else
   `reject_always`, into `PermissionDecision.option_id` (the field exists,
   today always `None` on deny — `permission.rs:26–27`); `None` only when no
   reject option was offered.

2. In `transport.rs` (`route_message`, `:465–481`), reply
   `Selected { option_id }` whenever the decision carries an option id —
   allow or deny — and `Cancelled` only otherwise. The audit entry already
   records `option_id` (`transport.rs:508`).

3. Add tests:

   ```rust
   #[test]
   fn deny_selects_offered_reject_option() { /* three-option fixture (permission.rs:149) → option_id "cancel" (reject_once) */ }
   #[tokio::test]
   async fn transport_replies_selected_on_deny_with_reject_option() { /* duplex peer sees {"outcome":"selected","optionId":"cancel"}, not cancelled */ }
   ```

- **Depends on:** location-aware-worktree-policy
- **Done when:** both tests pass;
  `permission_response_serializes_to_exact_acp_outcome_shape`
  (`protocol.rs:1013`) still passes; a deny against an options list with no
  reject kinds still answers `Cancelled`; cargo test/clippy/fmt green.

---

## 0075 — Audit correlation

### session-audit-identity — Entries born with `run_uid` + `task_id`

The transport writes placeholders (`transport.rs:503–504`); enrichment is an
exact-`PathBuf` registry lookup (`audit.rs:286–294`) that **discards** the
entry on a miss (`audit.rs:364–373`).

**Steps:**

1. In `crates/makina-core/src/backend.rs`, add `run_uid: Option<String>` and
   `task_id: Option<String>` to `SessionConfig` (`backend.rs:76–118`), both
   `#[serde(default)]`.

2. Thread them through `session_config_for` (`roles.rs:194–211`) and its
   call sites — `actors/developer.rs:194` and the Reviewer twin — sourcing
   the values the Supervisor already registers (`ctx.run_uid` / task id,
   `supervisor.rs:1622–1628`); extend the dispatch messages/actor args as
   needed.

3. In `crates/makina-acp`, carry the pair on `AcpCommand` (builder mirroring
   `with_audit_sink`, `client.rs:141–147`), through `AcpBackend::spawn` →
   `command_for` (`backend.rs:263`, `:201–209`) and into `Transport::new`;
   write the real ids at the entry construction (`transport.rs:501–514`),
   placeholders only when absent.

4. In `crates/makina-core/src/governance.rs`, add optional
   `run_uid` to `AuditEntry` (`governance.rs:60–83`); in
   `JsonlAuditSink::record` (`audit.rs:358–430`), route by `entry.run_uid`
   when present (no registry lookup), fall back to the registry otherwise —
   warn-and-discard now only applies to identity-less entries.

5. Add tests:

   ```rust
   #[tokio::test]
   async fn audit_entry_born_with_run_uid_and_task_id() { /* capturing sink behind the trait (tests/backend_trait.rs:455 pattern); SessionConfig carries ids; entry has them with no registry */ }
   #[tokio::test]
   async fn jsonl_sink_routes_by_entry_run_uid_without_registry() { /* identity-bearing entry lands in .makina/runs/{run_uid}/audit.jsonl with zero register() calls; identity-less entry still uses the fallback */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `jsonl_audit_sink_enriches_and_appends`
  (`audit.rs:519`) and `unregistered_working_dir_is_silently_discarded`
  (`audit.rs:667`) still pass (fallback unchanged); pre-0024 serialized
  configs/entries still deserialize (`serde(default)`); cargo
  test/clippy/fmt green.

### real-tool-name-in-audit — Stop recording the kind as the name

`ToolRef.name` is filled with the tool **kind** or the literal `"tool"`
(`transport.rs:486–495`), duplicating `ToolRef.kind` (`governance.rs:21–32`).

**Steps:**

1. At the `ToolRef` construction (`transport.rs:486–495`), derive `name`
   best-effort: the `toolCallId` prefix when it matches the observed
   `<name>__<suffix>` shape (real capture
   `"write_file__write_file_1780041520414_0"`, `protocol.rs:952`), else the
   title, else the kind, else `"tool"`. Keep `kind` as the kind. Document the
   heuristic at the site.

2. Add a test:

   ```rust
   #[tokio::test]
   async fn audit_records_real_tool_name() { /* capture-shaped toolCallId → name "write_file", kind Some("edit") */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; existing audit assertions
  (`transport.rs:745`, `tests/backend_trait.rs:482`, `backend.rs:759`)
  updated where they relied on the old `name == kind` behaviour; cargo
  test/clippy/fmt green.

---

## 0076 — Sandbox teeth

### harden-docker-gate-args — `--network=none` by default, caps, safe mounts

The Docker branch (`gate.rs:188–202`) bind-mounts RW via
`format!("{wd}:{wd}")` (`gate.rs:194–195`) from `to_string_lossy()`
(`gate.rs:190`) with no isolation flags; the only docker test
(`sandboxed_gate_uses_docker`, `gate.rs:364–377`) tolerates every outcome, so
hardening must be unit-testable without a docker daemon.

**Steps:**

1. In `crates/makina-core/src/config.rs`, extend `GateConfig`
   (`config.rs:364–380`) with `network: bool` (default `false`),
   `memory: Option<String>`, `cpus: Option<String>` (all `serde(default)`);
   `Config::validate` warns when they are set without `image`. Update the
   `[[gates]]` doc example (`config.rs:352–362`).

2. In `crates/makina-core/src/gate.rs`, extract the argv construction into
   `fn docker_run_args(gate: &GateConfig, working_dir: &Path) ->
   Result<Vec<String>, GateRunnerError>`: `run --rm`, `--network=none`
   unless `gate.network`, `--memory`/`--cpus` when set, `-v {wd}:{wd}`,
   `-w {wd}`, image, `sh -c {command}`. Reject non-UTF-8 paths
   (`Path::to_str() == None`) and paths containing `:` or `,` with a
   descriptive `GateRunnerError::Launch`. `run_gates` (`gate.rs:180–230`)
   consumes the builder. Note plan 0019 as the upstream guarantee for the
   `{plan_slug}--{task_id}` segment; this validation is defense-in-depth.

3. Add unit tests (pure argv assertions, no docker — same philosophy as
   `tests/gate_runner.rs`, which uses only shell builtins):

   ```rust
   #[test]
   fn docker_args_default_to_network_none() { /* argv contains "--network=none" */ }
   #[test]
   fn docker_args_honor_network_opt_out() { /* network = true → no --network flag */ }
   #[test]
   fn docker_args_include_resource_caps() { /* memory "2g", cpus "2" → "--memory","2g","--cpus","2" */ }
   #[test]
   fn docker_args_reject_colon_path() { /* "/tmp/evil:dir" → Err(Launch) with a clear message */ }
   ```

- **Depends on:** —
- **Done when:** all four tests pass; `sandboxed_gate_uses_docker`
  (`gate.rs:364`) still passes as the smoke test; host-exec gates
  (`gate.rs:204–207`) are byte-for-byte unaffected (existing
  `tests/gate_runner.rs` suite green); cargo test/clippy/fmt green.

### document-gate-trust-model — Say out loud what is and isn't sandboxed

**Steps:**

1. In the README's gate/configuration section and the `GateConfig` doc
   comment (`config.rs:346–362`), document: host-exec gates (no `image`) run
   the operator's own command on the host **by design** and are trusted
   operator input (plan 0008's locked decision); `image` gates are isolated
   with no network by default, an explicit `network = true` opt-out for
   dependency-fetching gates, and opt-in `memory`/`cpus` caps; the worktree
   is mounted read-write because gates must build in it.

2. Cross-link the residual-risk notes: agent-process sandboxing is future
   work (`doc/plan/0001-Initial/FUTURE.md:52`), slug validation is plan 0019.

- **Depends on:** harden-docker-gate-args
- **Done when:** the README and `config.rs` docs describe the trust model and
  every knob added by this plan with a `makina.toml` example; `cargo doc`
  renders without new warnings; cargo test/clippy/fmt green.

---

**End of plan 0024 TASKS.** When every "Done when" bullet is green, a tool
call writing outside its worktree is denied via the agent's own reject option
(and the deny branch is finally reachable in production), every audit entry
names its real run, task, and tool, and a sandboxed gate runs with no network,
optional resource caps, and a mount spec that cannot be silently malformed.
