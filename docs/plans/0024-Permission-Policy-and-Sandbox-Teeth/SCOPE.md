# Scope — Plan 0024

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Governance is the product's wedge, but the shipped permission policy is
**vacuous** and the "Docker sandbox" gate isn't isolation. This plan is the
follow-up to plan 0008 (Gate Sandboxing), which locked Docker + opt-in
`image` per gate but shipped no isolation flags, and to the permission/audit
gateway of plan 0002 (see also `docs/spec/acp-auth.md` for the trust model this
plan leaves intact: the agent *CLI* is operator-trusted; its *tool calls* are
not).

1. **The policy can never say no.** `WorktreePolicy::decide` checks
   `ctx.working_dir == self.worktree` (`permission.rs:97`) — but both sides
   derive from the same `command.working_dir`: the default policy is built
   from it (`client.rs:151–156`) and the very same path is handed to
   `Transport::new` (`client.rs:767–771`), which copies it into the request
   context (`transport.rs:457–462`). The comparison is a tautology; the deny
   branch (`permission.rs:123–131`) is unreachable outside hand-built tests.
   The module docs admit the policy ignores tool-call payload paths entirely
   (`permission.rs:77–80`). Net effect: **auto-approve anything that offers
   `allow_once`** — including writes far outside the worktree. The captured
   real payload in our own test suite is a write to `/tmp/fs-probe.txt`
   (`protocol.rs:956–958`), which this "worktree policy" happily allows.
2. **The data to do better is already on the wire.** `ToolCall`'s flatten map
   preserves every unknown field — `locations`, `content` diffs, `_meta`
   (`protocol.rs:629–631`) — proven against a real `gemini --acp` capture by
   `request_permission_params_deserializes_real_payload_and_preserves_unknown_tool_call_fields`
   (`protocol.rs:938–1010`, locations asserted at `:991–994`). A
   location-aware policy is implementable today, no protocol change needed.
3. **Denial is expressed as cancellation.** When the policy denies, the
   transport replies `PermissionOutcome::Cancelled` (`transport.rs:477–481`)
   even when the agent offered a `reject_once`/`reject_always` option (kinds
   already modelled, `protocol.rs:603–610`). Some agents treat *cancelled* as
   "the user aborted the turn" rather than "this tool call was refused —
   continue differently".
4. **Audit correlation is brittle and the tool identity is wrong.** The
   transport records `ToolRef.name` = the tool **kind** (`"edit"`/`"execute"`,
   fallback literal `"tool"`) (`transport.rs:486–495`) and placeholder ids
   (`run_id: "acp-transport"`, `task_id: None`, `transport.rs:503–504`).
   Enrichment exists — `JsonlAuditSink` maps `working_dir → (run_uid, run_id,
   slug, task_id)` (`audit.rs:358–383`), registered by the Supervisor before
   dispatch (`supervisor.rs:1622–1628`) — but it hinges on **exact `PathBuf`
   byte-equality** (documented caveat, `audit.rs:286–294`) and **silently
   discards** the entry on a miss (`audit.rs:364–373`). One canonicalization
   mismatch and the record is gone; and even when it lands, `name == kind`
   means the ledger cannot say *which tool* ran.
5. **The Docker gate has no teeth.** The sandbox branch
   (`crates/makina-core/src/gate.rs:188–202`) bind-mounts the worktree
   **read-write** via `format!("{wd}:{wd}")` (`gate.rs:194–195`) with no
   `--network=none`, no `--read-only`, no resource caps — the image label is
   the only isolation, so a gate triggered by agent-authored build config
   (plan 0008's own threat model) can exfiltrate over the network or chew the
   host's resources. The mount spec is also built from
   `working_dir.to_string_lossy()` (`gate.rs:190`): a path containing `:`
   (or non-UTF-8 bytes) produces a broken/ambiguous `-v` argument, and the
   path embeds the *plan slug* (`.makina/worktrees/{plan_slug}--{task_id}`,
   per plan 0008) — whose validation lands in **plan 0019**, a prerequisite
   for trusting that segment.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0074–0076):

- **0074 — Location-aware policy v2.** Parse the tool call's `locations` /
  `content` paths out of the preserved extras into a typed view (a
  `protocol.rs` helper); allow only when every referenced path is inside the
  session worktree, deny when any falls outside; absent path data follows a
  configurable default; denial selects an offered `reject_*` option before
  falling back to `Cancelled`.
- **0075 — Audit correlation.** Thread `run_uid` + `task_id` from the
  orchestrator into `SessionConfig` → `AcpCommand` → the transport's audit
  context so entries are born with their real identity (the working-dir
  registry becomes a fallback, and a lookup miss no longer destroys the
  record); record a best-effort real tool name alongside the kind.
- **0076 — Sandbox teeth.** Docker gate runs get `--network=none` by default
  with a per-gate `network = true` opt-out, opt-in resource caps
  (`--memory` / `--cpus`), safe mount-spec construction (reject paths
  unrepresentable in `-v`), and explicit documentation that host-exec gates
  are trusted operator input.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `WorktreePolicy` compares a value to itself; payload paths ignored; auto-approves out-of-worktree writes | `0074` |
| `locations`/path data already round-trips through the flatten map | `0074` |
| Deny answered as `Cancelled` even when a `reject_*` option is offered | `0074` |
| Audit identity depends on exact-path registry lookup that drops on miss; `ToolRef.name` duplicates the kind | `0075` |
| Docker gate: RW mount, no network/resource isolation, `:`-unsafe mount spec, unvalidated slug in the path | `0076` |

## Locked decisions

- **Path scoping is lexical, not canonicalizing.** Referenced paths are
  component-normalized (`.`/`..` resolved textually, relative paths resolved
  against the worktree) and prefix-checked against the worktree path.
  `canonicalize` would fail on not-yet-created targets (diff destinations)
  and still wouldn't stop a symlink planted *inside* the worktree — symlink
  escapes are explicitly residual risk, deferred to OS-level enforcement
  (`docs/plans/0001-Initial/FUTURE.md:52`, "Hard enforcement via sandboxing").
- **Missing path data allows; malformed path data denies.** Tool calls with
  *no* locations/content paths (the normal shape for `execute`/terminal
  calls) keep today's behaviour — denying them would kill every shell command
  and regress the permission-hang fix. Path data that is *present but
  unparseable* is treated as evasion and denied. A `strict_paths` knob flips
  "missing" to deny for hardened deployments. Rationale recorded in the
  policy's docs.
- **Deny prefers an offered reject option.** Selection order:
  `reject_once` → `reject_always` → `Cancelled` fallback (only when the agent
  offered no reject option). `PermissionDecision.option_id` carries the
  choice on the deny path too; the audit entry already records `option_id`.
- **Audit identity travels with the session.** `SessionConfig` gains optional
  `run_uid` / `task_id` (`#[serde(default)]`, backward-compatible), flowing
  `session_config_for` (`roles.rs:194–211`) → `AcpBackend::spawn`
  (`backend.rs:263`) → `AcpCommand` → transport. `AuditEntry` gains an
  optional `run_uid` so `JsonlAuditSink::record` can route without a registry
  hit; the registry remains as enrichment fallback and the drop-on-miss path
  only applies to entries that carry no identity of their own.
- **Network off by default; resource caps opt-in.** `--network=none` unless
  the gate sets `network = true` (some gates legitimately fetch dependencies);
  `memory` / `cpus` are per-gate opt-in with **no** built-in default — a wrong
  default cap turns passing gates into flaky OOM failures, which is worse
  than no cap and erodes trust in the gate signal.
- **Unrepresentable mount paths are launch errors.** A worktree path
  containing `:` or non-UTF-8 bytes is rejected with a clear
  `GateRunnerError::Launch` instead of constructing an ambiguous `-v` spec.
  Plan 0019's slug validation is the upstream guarantee for the
  `{plan_slug}--{task_id}` segment; this check is defense-in-depth for the
  repo-root prefix the operator controls.
- **Host-exec gates stay trusted.** A gate without `image` runs the
  operator's own command on the host *by design* (plan 0008 locked sandboxing
  as opt-in). This plan documents that contract explicitly rather than
  changing it.

## Out of scope

- OS-level (non-Docker) sandboxing of the **agent process** itself —
  filesystem/network namespaces around the CLI
  (`docs/plans/0001-Initial/FUTURE.md:52`, "Hard enforcement via sandboxing";
  plan 0008 explicitly deferred it too).
- Per-task gate overrides (gates remain per-project `makina.toml` config).
- A policy DSL / config-file rule language beyond the worktree boundary +
  the `strict_paths` and `network` knobs.
- Symlink-resolution enforcement inside the worktree (residual risk,
  documented; see locked decisions).
- ACP transport mechanics — timeouts, id fidelity, write-path decoupling
  (plan 0021; both plans touch `transport.rs:451–559`, coordinate if they
  land close together).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
