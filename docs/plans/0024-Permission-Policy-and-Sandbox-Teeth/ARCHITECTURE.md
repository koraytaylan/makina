# Architecture — Plan 0024 (deltas)

> Edits in `crates/makina-acp/src/permission.rs`, `crates/makina-acp/src/
> protocol.rs`, `crates/makina-acp/src/transport.rs`, `crates/makina-acp/src/
> client.rs`, `crates/makina-core/src/backend.rs`, `crates/makina-core/src/
> roles.rs`, `crates/makina-core/src/governance.rs`, `crates/makina-core/src/
> audit.rs`, `crates/makina-core/src/gate.rs`, `crates/makina-core/src/
> config.rs`, and the actor call sites (`actors/developer.rs` /
> `actors/reviewer.rs`, `actors/supervisor.rs`). Line numbers are hints;
> locate by symbol.

## 0074 — Location-aware policy v2

Today `WorktreePolicy::decide` compares `ctx.working_dir` to the worktree it
was constructed from (`permission.rs:97`) — both derive from
`command.working_dir` (`client.rs:151–156`, `client.rs:767–771`,
`transport.rs:457–462`), so the deny branch (`permission.rs:123–131`) is dead
code in production and payload paths are ignored by admission
(`permission.rs:77–80`).

Edits:

- **Typed path view on `ToolCall`** (`protocol.rs:619–632`): a helper that
  reads the preserved extras —

  ```rust
  pub enum PathExtraction {
      /// No locations/content path data present (normal for execute calls).
      Absent,
      /// Every referenced path parsed cleanly.
      Paths(Vec<PathBuf>),
      /// Path-shaped data present but unparseable (treated as evasion).
      Malformed,
  }

  impl ToolCall {
      pub fn referenced_paths(&self) -> PathExtraction { … }
  }
  ```

  Sources, matching the real capture (`protocol.rs:944–963`):
  `extra["locations"]` (array of `{ "path": … }`) and `extra["content"]`
  entries carrying a `"path"` (diff blocks). Anything else in `extra` is
  ignored. Unit-test against the captured payload that already lives in
  `protocol.rs:938–1010`.

- **Policy decision** (`permission.rs:95–136`): `WorktreePolicy` gains a
  `strict_paths: bool` (default `false`) and decides per tool call:
  1. `Paths(ps)` and every `p` lexically inside the worktree → allow
     (`allow_once` selection unchanged, `permission.rs:100–113`);
  2. `Paths(ps)` with any path outside → **deny**, reason names the
     offending path;
  3. `Malformed` → deny;
  4. `Absent` → allow unless `strict_paths` (rationale in the doc comment:
     execute/terminal calls carry no paths; deny-on-absent kills every shell
     command).
  "Lexically inside" = normalize components (`.` dropped, `..` popped,
  relative paths joined onto the worktree) then `starts_with(worktree)`. No
  filesystem access; symlink escape is documented residual risk. Rewrite the
  MVP-limitation note (`permission.rs:77–80`) to describe the new behaviour.
  The tautological `ctx.working_dir == self.worktree` check is retained only
  as a sanity guard (mismatch → deny + warn, it indicates mis-wiring).

- **Deny selects a reject option** (`permission.rs` + `transport.rs:465–481`):
  on deny, the policy picks `reject_once`, else `reject_always`
  (`PermissionOptionKind`, `protocol.rs:603–610`), placing it in
  `PermissionDecision.option_id` (the field exists, today always `None` on
  deny — `permission.rs:26–27`). The transport's response construction
  becomes: `Selected { option_id }` whenever the decision carries one —
  allow *or* deny — and `Cancelled` only when it doesn't. The audit entry
  already records `option_id` (`transport.rs:508`).

## 0075 — Audit correlation

The transport emits placeholders (`run_id: "acp-transport"` at
`transport.rs:503`, `task_id: None` at `:504`) and `ToolRef.name` = the kind
or the literal `"tool"` (`transport.rs:486–495`). `JsonlAuditSink` enriches by
exact-`PathBuf` registry lookup (`audit.rs:358–383`; caveat documented at
`audit.rs:286–294`) and **discards on miss** (`audit.rs:364–373`).

Edits:

- **`SessionConfig` carries identity** (`backend.rs:76–118`): add

  ```rust
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub run_uid: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub task_id: Option<String>,
  ```

  `session_config_for` (`roles.rs:194–211`) gains the two values (new
  parameters or a small `AuditIdentity` arg); the call sites in the Developer
  (`actors/developer.rs:194`) and its Reviewer twin fill them from the
  dispatch context — the Supervisor already owns both (`ctx.run_uid`,
  `task_id`, exactly what it registers at `supervisor.rs:1622–1628`); thread
  them into the `Develop`/`Review` messages or actor args alongside the
  existing worktree path.

- **`AcpCommand` → transport audit context**: `AcpBackend::spawn`
  (`backend.rs:263–265`) passes `config.run_uid`/`config.task_id` into
  `command_for` → a new `AcpCommand { audit_identity: Option<(String,
  String)> }` (builder, mirroring `with_audit_sink`,
  `client.rs:141–147`) → `Transport::new` stores it → the entry construction
  (`transport.rs:501–514`) writes the real ids when present, placeholders
  otherwise (test transports).

- **`AuditEntry.run_uid`** (`governance.rs:60–83`): optional,
  `#[serde(default)]`. `JsonlAuditSink::record` (`audit.rs:358–430`) routes by
  `entry.run_uid` when present (path via `paths::audit_log`, `audit.rs:402`)
  and only falls back to the registry for identity-less entries; the
  warn-and-discard branch applies solely to that fallback. Registry,
  `register`, and `evict_run` stay as-is — they remain the safety net for
  any producer that cannot carry identity.

- **Real tool name, best-effort** (`transport.rs:486–495`): populate
  `ToolRef.name` from the `toolCallId` prefix when it matches the observed
  `<name>__<suffix>` shape (real capture:
  `"write_file__write_file_1780041520414_0"`, `protocol.rs:952`), else the
  title, else the kind, else `"tool"`. `ToolRef.kind` keeps the kind
  (`governance.rs:21–32` — the fields already exist; only the population is
  wrong). Document the heuristic at the construction site.

## 0076 — Sandbox teeth

The Docker branch (`gate.rs:188–202`) is argv built inline: RW `-v {wd}:{wd}`
(`gate.rs:194–195`) from `to_string_lossy()` (`gate.rs:190`), `-w`, image,
`sh -c`. No network/resource flags. The only docker test
(`sandboxed_gate_uses_docker`, `gate.rs:364–377`) tolerates every outcome
because docker may be absent, and `tests/gate_runner.rs` has no docker
coverage at all — so the hardening must be testable **without docker**.

Edits:

- **`GateConfig` knobs** (`config.rs:364–380`):

  ```rust
  /// Allow network inside the sandbox (default false → --network=none).
  #[serde(default)]
  pub network: bool,
  /// Optional docker --memory limit (e.g. "2g"). Opt-in; no default cap.
  #[serde(default)]
  pub memory: Option<String>,
  /// Optional docker --cpus limit (e.g. "2"). Opt-in; no default cap.
  #[serde(default)]
  pub cpus: Option<String>,
  ```

  All three are ignored for host-exec gates (no `image`); `Config::validate`
  warns when they are set without `image`. Update the `[[gates]]` example in
  the doc comment (`config.rs:352–362`) and the README/config docs, including
  an explicit note that **host-exec gates are trusted operator input** (they
  run the operator's own command on the host by design — plan 0008's locked
  decision).

- **Pure argv builder** (`gate.rs`): extract the inline construction into

  ```rust
  fn docker_run_args(gate: &GateConfig, working_dir: &Path)
      -> Result<Vec<String>, GateRunnerError>
  ```

  producing `run --rm` + `--network=none` (unless `gate.network`) +
  `--memory M` / `--cpus C` (when set) + `-v {wd}:{wd}` + `-w {wd}` +
  `{image} sh -c {command}`. It **rejects** a `working_dir` that is non-UTF-8
  (`Path::to_str` is `None`) or contains `:`/`,` with a descriptive
  `GateRunnerError::Launch` instead of emitting an ambiguous mount spec.
  `run_gates` (`gate.rs:180–230`) calls the builder and feeds the args to
  `tokio::process::Command::new("docker")`. Note the dependency: plan 0019's
  slug validation guarantees the `{plan_slug}--{task_id}` path segment
  upstream; this check is defense-in-depth for the rest of the path.

## Test strategy

- `referenced_paths_extracts_locations_and_diff_paths` (`protocol.rs`): run
  the helper over the captured gemini payload (`protocol.rs:944–963`) →
  `Paths([/tmp/fs-probe.txt, /tmp/fs-probe.txt])`; a payload without path
  data → `Absent`; `locations: [{"path": 42}]` → `Malformed`.
- `out_of_worktree_path_is_denied` (`permission.rs`): worktree
  `/wt/task-a`, tool call writing `/tmp/evil` → deny, reason names the path —
  the deny branch is finally reachable with production wiring.
- `in_worktree_paths_are_allowed` and
  `relative_and_dotdot_paths_normalize_before_check` (`permission.rs`):
  `src/main.rs` allows; `src/../../escape` denies.
- `absent_paths_follow_strict_knob` (`permission.rs`): `Absent` allows by
  default, denies with `strict_paths`.
- `deny_selects_offered_reject_option` (`permission.rs` +
  `transport.rs` inline): with the three-option fixture
  (`permission.rs:149–177`) a deny carries `option_id == "cancel"`
  (`reject_once`); the transport replies `Selected`, not `Cancelled`; an
  options list without reject kinds falls back to `Cancelled`.
- `audit_entry_born_with_run_uid_and_task_id` (`tests/backend_trait.rs`,
  extending the capturing-sink pattern at `:455–544`): a `SessionConfig` with
  `run_uid`/`task_id` set yields an entry carrying them with no registry
  involved.
- `jsonl_sink_routes_by_entry_run_uid_without_registry` (`audit.rs` tests,
  beside `jsonl_audit_sink_enriches_and_appends`, `audit.rs:519`): an
  identity-bearing entry lands in `.makina/runs/{run_uid}/audit.jsonl` with
  zero registrations; an identity-less one still uses the registry fallback
  (and still warn-discards on miss).
- `audit_records_real_tool_name` (`transport.rs` inline): the
  `write_file__…` capture shape records `name == "write_file"`,
  `kind == Some("edit")`.
- `docker_args_default_to_network_none`, `docker_args_honor_network_opt_out`,
  `docker_args_include_resource_caps`, `docker_args_reject_colon_path`,
  `docker_args_reject_non_utf8_path` (`gate.rs` unit tests): pure assertions
  on the builder's argv — no docker daemon needed, matching how
  `tests/gate_runner.rs` avoids env-dependent tools. The existing
  `sandboxed_gate_uses_docker` smoke test (`gate.rs:364–377`) stays.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- **Plan 0008:** direct follow-up — keeps its locked decisions (Docker,
  opt-in `image` per gate) and adds the isolation flags it deferred; its
  out-of-scope (sandboxing the agent CLI) stays out of scope here too.
- **Plan 0002 (governance/audit):** extends, never rewrites: `AuditEntry`
  gains one optional field, `JsonlAuditSink` gains a routing fast-path, the
  Supervisor's `register` call (`supervisor.rs:1622`) is unchanged.
- **Plan 0019 (slug safety):** prerequisite for fully trusting the
  `{plan_slug}--{task_id}` mount path segment; 0076's builder-side validation
  is the defense-in-depth layer and does not depend on 0019 to land.
- **Plan 0021 (ACP timeouts/turn hygiene):** disjoint goals, overlapping
  file region — both edit the permission-answer path
  (`transport.rs:451–559`; 0021 re-plumbs it through a writer task, this plan
  changes the decision payload). Whichever lands second rebases that region.
- **Plan 0015:** no overlap.
