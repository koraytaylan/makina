# Makina Trial — Dogfood Task List

Structured-text trial task list for exercising the Makina full loop
(Planner → Supervisor → Developer + gates → Reviewer → squash-merge).
Tasks target the `makina-core` crate and are deliberately small,
self-contained, and safe — they add new utility code without touching
any existing actor, FSM, or orchestration module.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- The Planner adds further dependency edges automatically.
- **Done when** is the acceptance check used by gates and the Reviewer.

---

## 0001 — Utility Helpers

### format-duration — Add a human-readable duration formatter

Add a pure function `format_duration(secs: u64) -> String` to a new
`makina_core::util` module. The function must format a duration in
seconds as a compact human-readable string: `"0s"` for zero, `"42s"`
for values under a minute, `"3m 05s"` for values under an hour, and
`"1h 02m 03s"` for values of one hour or more. The module must be
declared `pub mod util;` in `makina_core::lib.rs` and the function
must carry a doc-comment.
- **Depends on:** —
- **Done when:** `pub fn format_duration(secs: u64) -> String` exists
  in `crates/makina-core/src/util.rs`; unit tests cover the cases 0,
  42, 185 (3m 05s), and 3723 (1h 02m 03s); `cargo test -p makina-core`,
  `cargo clippy -p makina-core -- -D warnings`, and
  `cargo fmt --check -p makina-core` all pass.

### kebab-validate — Add a kebab-id validation predicate

Add a pure function `is_valid_kebab_id(s: &str) -> bool` to the same
`makina_core::util` module (created by `format-duration`). The
function must enforce the task-id rules from the structured-text
convention: one or more `[a-z0-9]` segments joined by single hyphens,
minimum two characters, must start and end with a lowercase letter or
digit, no consecutive hyphens. The function must carry a doc-comment.
- **Depends on:** format-duration
- **Done when:** `pub fn is_valid_kebab_id(s: &str) -> bool` exists in
  `crates/makina-core/src/util.rs`; unit tests assert `true` for
  `"ab"`, `"workspace-scaffold"`, `"task-model"`, `"e2e-run"` and
  `false` for `""`, `"A"`, `"-task"`, `"task-"`, `"task--model"`,
  `"t"`, `"WorkspaceScaffold"`; `cargo test -p makina-core`,
  `cargo clippy -p makina-core -- -D warnings`, and
  `cargo fmt --check -p makina-core` all pass.
