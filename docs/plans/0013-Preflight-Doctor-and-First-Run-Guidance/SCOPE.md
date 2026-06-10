# Scope — Plan 0013

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The first five minutes with Makina are the worst-supported part of the product.
A new user who runs `cargo run -p makina` before writing config is met with a
single cryptic line and an immediate exit:

```
failed to load configuration: backend.command must not be empty
```

(`crates/makina/src/main.rs:42`). That message is identical whether the user has
**no** config at all, a **typo** in one of two TOML files, or a **role pointing
at a provider that doesn't exist** — and it never says *which file* to edit,
*where* the file lives, or *what a minimal valid config looks like*. Worse, even
a valid config can fail at the first run: if the configured agent binary
(`gemini`, `grok`, …) isn't on `PATH` or was never signed in, nothing checks it
up front — the task simply spawns, fails (or hangs), and the user is left
guessing from a `[✗ failed]` badge.

Three concrete gaps, all in the onboarding path:

1. **Config errors are not actionable.** `ConfigError::Parse` and
   `ConfigError::Validation` (`config.rs:47`, `:590`) carry a reason but not the
   originating file, the merge precedence, or a fix. The unknown-provider error
   (`config.rs:622`) names the bad provider but not the *valid* ones.
2. **No provider preflight.** Nothing validates that a provider's `command`
   resolves on `PATH` before a run tries to use it. The failure surfaces late,
   deep in a task, with no hint that the cause is "binary not installed."
3. **No first-run guidance or in-app doctor.** There is no `makina init`, no
   sample-config scaffold, and no in-TUI health view that tells the user what is
   and isn't configured correctly.

This plan makes the onboarding path *legible*: precise config errors, an
up-front provider preflight, and an in-app **Doctor** view that can also scaffold
a starter config when none exists.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0044–0046):

- **0044 — Actionable config errors.** Enrich `ConfigError` surfacing so every
  parse/validation failure names the **file** it came from, states the
  global-vs-project **precedence**, and (for the unknown-provider case) lists the
  **providers that *are* defined**. Keep the existing `reason` strings; add
  context around them.
- **0045 — Provider binary preflight.** Add a `makina-core` helper that resolves
  each provider's `command` against `PATH` (and optionally probes `--version`),
  returning a structured per-provider result. Run it at startup and surface a
  clear, non-fatal warning panel for any provider whose binary is missing.
- **0046 — In-app Doctor view + first-run scaffold.** A `[?]`/doctor overlay that
  lists each health check (config files found, providers resolvable, base branch
  exists, git worktree clean) with pass/fail, and — when **no** config is found —
  offers to write a commented starter `~/.makina/config.toml` and
  `.makina/config.toml` from templates.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Cryptic, file-less, fix-less config errors | `0044` |
| No check that the agent binary exists before a run uses it | `0045` |
| No first-run guidance, no `init`, no in-app health view | `0046` |

## Locked decisions

- **Don't change the config *schema* or validation *rules*.** 0044 only enriches
  how existing `ConfigError` values are *presented*; the `validate()` rules
  (`config.rs:590`) are unchanged.
- **Preflight is advisory, never fatal.** A missing binary produces a warning the
  user can act on; it does not block startup or abort a run (the user may be
  about to install it, or only some providers may be needed for the selected
  run). The existing late-spawn error path remains the backstop.
- **No new dependency for `PATH` resolution.** Resolve `command` by walking
  `$PATH` with `std`/`tokio` primitives (the same way a shell would for the first
  token of the command), not by adding a `which` crate.
- **Permission/auth is out of scope here.** Auto-approval via `WorktreePolicy`
  already exists (`makina-acp/src/permission.rs`); this plan does not touch the
  permission flow. "Signed in?" is *agent-owned* state Makina cannot reliably
  introspect — the doctor reports *binary presence*, not auth status, and points
  the user at the agent's own sign-in step.

## Out of scope

- Implementing the ACP `session/request_permission` flow (already done).
- Idle/hang detection and live-activity feedback (plan 0015).
- Surfacing per-task failure reasons in the task view (plan 0014).
- Any non-ACP / direct-API backend support.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
