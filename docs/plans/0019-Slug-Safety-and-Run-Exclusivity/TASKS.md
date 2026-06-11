# Makina Plan 0019 — Slug Safety & Run Exclusivity

Introduce a validated `Slug` newtype so no unvalidated slug ever becomes a
filesystem path or branch name (closing the model-controlled
`graph.slug` → `.makina/tasks/` write path and the unvalidated `plan_slug`
worktree path), and refuse to start a second live run on a plan slug so the
worktree reclaim heuristic stays sound.

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

## 0060 — Validated `Slug` newtype

### slug-newtype — Add `Slug` with §4.1 validation and string serde

`TaskGraph.slug` is a bare `String` (`task.rs:189–191`) that
`parse_model_response` (`interpreter.rs:780–794`) accepts verbatim from the
model and `persist_graph` writes into a path (`persist.rs:176`, `:187`).

**Steps:**

1. In `crates/makina-core/src/task.rs`, next to `TaskId` (`task.rs:52`), add
   `pub struct Slug(String)` + `SlugError` with `Slug::parse` enforcing
   artifact-schema §4.1 (`docs/spec/runtime-artifact-schema.md` §4.1):
   `[a-z0-9-]` only, starts and ends alphanumeric, no `--`, length ≥ 2.
   Implement `TryFrom<String>`, `From<Slug> for String`, `Display`,
   `AsRef<str>`, `as_str()`, and serde via
   `#[serde(try_from = "String", into = "String")]`.

2. Change `TaskGraph.slug` to `Slug` and fix the in-crate construction sites
   the compiler flags (test fixtures construct via `Slug::parse(..).unwrap()`).
   `TaskGraph::validate` (`task.rs:218–242`) is unchanged.

3. Add unit tests:

   ```rust
   #[test]
   fn slug_rejects_traversal_and_unsafe_chars() { /* "../../../tmp/evil", "a/b", "a..b", "A-B", "a--b", "-a", "a-", "", "a" rejected; "0005-tui-ingestion-responsiveness-tasks", "my-feature", "ab" accepted */ }
   #[test]
   fn slug_serde_roundtrips_as_plain_string() { /* TaskGraph slug serializes as a bare string; "slug": "../evil" fails to deserialize */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; a `TaskGraph` with an unsafe slug is
  unrepresentable after deserialization; existing artifacts
  (`.makina/tasks/0005-tui-ingestion-responsiveness-tasks.json`) still load;
  cargo test/clippy/fmt green.

### thread-slug-through-paths — `&Slug` at every path/branch construction site

**Steps:**

1. Change `paths::worktree` (`paths.rs:120–125`) and `paths::task_graph` to
   take `plan_slug: &Slug` / `slug: &Slug`; change `persist::tasks_path` /
   `temp_path` (`persist.rs:103–124`) and `load_graph` (`persist.rs:209`)
   likewise.

2. Change `WorktreeManager::create` / `remove` (`worktree.rs:190–215`,
   `:267–271`) to `plan_slug: &Slug` (the `worktree_path` helper at
   `worktree.rs:325–327` and branch `format!`s at `worktree.rs:198`, `:271`
   follow). `validate_task_id` (`worktree.rs:390–413`) stays for `task_id`.

3. Make `orchestrator::run_slug` / `plan_slug` (`orchestrator.rs:120–150`,
   `:163–177`) return `Slug` (output of `sanitize_kebab` + length check +
   `SLUG_FALLBACK` is §4.1-valid by construction; `expect` with a comment).
   Thread `Slug` through `RunEntry.run_slug`/`plan_slug`
   (`orchestrator.rs:730–740`), `start_run`'s extraction
   (`orchestrator.rs:856–919`), the supervisor's `DriverContext` fields
   (`supervisor.rs:475`, `:498`), the `driver_context` builder
   (`supervisor.rs:738–746`), the `run_graph` params
   (`supervisor.rs:837–839`), `DriverGuard.plan_slug`
   (`supervisor.rs:1445`), the task-branch `format!`
   (`supervisor.rs:1738`), and `RunMetadata.run_slug`
   (`run_metadata.rs:62–77`, same string serde). Display-only edges (audit
   registry, `supervisor.rs:1622–1628`) convert with `to_string()`.

4. Update test helpers (`tests/worktree.rs`, `tests/squash_merge.rs`,
   `tests/concurrency.rs`, orchestrator unit tests) to construct `Slug`s; the
   task-id validation tests (`tests/worktree.rs:335–399`) and
   `worktree_path_is_under_repo_root` (`worktree.rs:503–510`) stay green.

- **Depends on:** slug-newtype
- **Done when:** no `&str` slug parameter remains on any function that builds
  a filesystem path or branch name (grep `plan_slug: &str` / `slug: &str` in
  `makina-core/src` returns nothing); the full suite passes; cargo
  test/clippy/fmt green.

### reject-unsafe-model-slug — Typed rejection at the interpretation boundary

**Steps:**

1. In `crates/makina-core/src/interpreter.rs`, add
   `InterpretError::UnsafeSlug { slug: String, reason: String }` and, in
   `parse_model_response` (`interpreter.rs:780–794`), validate the extracted
   JSON's raw `"slug"` field with `Slug::parse` *before* the full `TaskGraph`
   deserialization, returning `UnsafeSlug` on failure (so the
   prompt-injection case gets a precise diagnosis instead of a generic
   `ModelResponseInvalid`).

2. Confirm the orchestrator's interpret-failure fold
   (`orchestrator.rs:819–836`) surfaces it as a blocking `interpreter-failed`
   ingestion issue (no new arm required; the run stays reviewable-Pending).

3. Add a test next to the existing `parse_model_response` suite
   (`interpreter.rs:936` onwards):

   ```rust
   #[test]
   fn model_supplied_unsafe_slug_is_rejected() { /* slug "../../../tmp/evil" → InterpretError::UnsafeSlug; a valid response still parses */ }
   ```

- **Depends on:** slug-newtype
- **Done when:** the test passes; a hostile model slug can neither
  deserialize into a `TaskGraph` nor reach `persist_graph`; cargo
  test/clippy/fmt green.

---

## 0061 — Per-slug run exclusivity

### start-run-slug-exclusivity — Refuse a second live run on a plan slug

The reclaim heuristic (`worktree.rs:209–216`) deletes any existing
`{plan_slug}--{task_id}` worktree/branch as "stale"; with two live runs on one
slug, run B reclaims run A's live worktree mid-task. `start_run`
(`orchestrator.rs:853`) currently performs no slug check.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, inside `start_run`'s registry
   critical section (`orchestrator.rs:856–919`), after the blocked-ingestion
   check (`:864–878`) and before the handle swap (`:882–891`): scan the
   registry for another entry (`id != run.0`) with the same `plan_slug` whose
   status is `RunStatus::Running` or `RunStatus::Paused` (`api.rs:206–217`);
   if found, return `ApiError::InvalidCommand` (`api.rs:386–390`) with a
   reason naming the live run id, the plan slug, and its status. Order the
   immutable scan before the `get_mut` borrow.

2. Document the invariant where the reclaim heuristic relies on it: extend the
   reclaim comment (`worktree.rs:204–216`) with "sound because `StartRun`
   enforces at most one live run per plan slug (plan 0019)".

3. Add orchestrator unit tests (NoopBackend harness, alongside the existing
   `start_run` tests):

   ```rust
   #[tokio::test]
   async fn second_start_on_same_slug_is_refused_until_terminal() { /* open same task list twice; start A; start B → InvalidCommand naming run A; A reaches terminal; start B → Ok */ }
   #[tokio::test]
   async fn paused_run_still_blocks_start_on_same_slug() { /* pause A; start B → refused; cancel/finalize A; start B → Ok */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; resuming the *same* paused run still works;
  two live runs can no longer coexist on one plan slug; cargo test/clippy/fmt
  green.

---

**End of plan 0019 TASKS.** When every "Done when" bullet is green, no slug —
model-supplied or otherwise — can escape `.makina/` as a path or branch name,
and the worktree reclaim heuristic only ever fires on genuinely stale slots.
