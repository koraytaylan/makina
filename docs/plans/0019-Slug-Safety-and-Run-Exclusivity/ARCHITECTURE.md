# Architecture — Plan 0019 (deltas)

> Edits in `crates/makina-core/src/task.rs` (the `Slug` newtype),
> `interpreter.rs` (boundary rejection), `paths.rs`, `persist.rs`,
> `worktree.rs`, `run_metadata.rs`, `orchestrator.rs` (threading +
> exclusivity), and `actors/supervisor.rs` (context fields). Tests in
> `tests/worktree.rs` and the `orchestrator.rs`/`interpreter.rs` unit suites.
> Line numbers are hints; locate by symbol.

## 0060 — Validated `Slug` newtype

### The type

In `task.rs`, next to `TaskId` (`task.rs:52`):

```rust
/// A validated kebab-case slug (artifact-schema §4.1): `[a-z0-9-]`, starts and
/// ends alphanumeric, no `--`, length ≥ 2. Safe by construction for use in
/// filesystem paths (`.makina/tasks/{slug}.json`,
/// `.makina/worktrees/{slug}--{task_id}`) and branch names
/// (`task/{slug}--{task_id}`); `--` is rejected so the worktree delimiter
/// stays unambiguous (`paths.rs:105–107`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Slug(String);

impl Slug {
    pub fn parse(s: &str) -> Result<Self, SlugError> { /* §4.1 rules */ }
    pub fn as_str(&self) -> &str { &self.0 }
}
// + TryFrom<String>, From<Slug> for String, Display, AsRef<str>.

#[derive(Debug, Error, PartialEq)]
#[error("invalid slug {slug:?}: {reason}")]
pub struct SlugError { pub slug: String, pub reason: String }
```

`TaskGraph.slug: String` (`task.rs:189–191`) becomes `pub slug: Slug`. The
`try_from`/`into` serde attributes keep the wire format a plain JSON string —
existing artifacts (`.makina/tasks/0005-tui-ingestion-responsiveness-tasks.json`)
deserialize unchanged — while *every* deserialization (model response,
persisted artifact) now validates. `TaskGraph::validate` (`task.rs:218–242`)
stays id/dep-focused; slug safety is the type's job.

### The model boundary

`parse_model_response` (`interpreter.rs:780–794`) gains a pre-check between
JSON extraction and full deserialization:

```rust
// Typed rejection for the prompt-injection case: pull the raw "slug" field
// and validate it BEFORE TaskGraph deserialization, so an unsafe slug is
// diagnosed as UnsafeSlug rather than a generic serde failure.
if let Some(raw_slug) = value.get("slug").and_then(|v| v.as_str())
    && let Err(e) = Slug::parse(raw_slug)
{
    return Err(InterpretError::UnsafeSlug {
        slug: raw_slug.to_string(),
        reason: e.reason,
    });
}
```

with the new variant on `InterpretError`. The orchestrator's interpret-failure
fold (`orchestrator.rs:819–836`) needs no new arm — `UnsafeSlug` lands in the
catch-all `interpreter-failed` blocking issue, which renders the message; a
dedicated issue code is optional polish.

### Threading `&Slug` through path/branch construction

Every site where a slug becomes a path or branch takes `&Slug`; display-only
edges convert with `as_str()`/`to_string()`:

- `paths::worktree(repo_root, plan_slug: &Slug, task_id)` (`paths.rs:120–125`)
  and a matching change to `paths::task_graph`.
- `persist::tasks_path` / `temp_path` (`persist.rs:103–124`) take `&Slug`;
  `persist_graph` / `load_graph` (`persist.rs:155`, `:209`) follow from
  `graph.slug: Slug`.
- `WorktreeManager::create` / `remove` (`worktree.rs:190–215`, `:267–271`):
  `plan_slug: &Slug` replaces the raw `&str`; `worktree_path`
  (`worktree.rs:325–327`) and the branch `format!` sites (`worktree.rs:198`,
  `:271`) follow. `validate_task_id` stays as-is for `task_id`.
- `orchestrator::run_slug` / `plan_slug` (`orchestrator.rs:120–150`,
  `:163–177`) return `Slug` (sanitizer output is valid by construction;
  `Slug::parse(...).expect(...)` with a comment, since `sanitize_kebab` +
  the ≥ 2 length check + `SLUG_FALLBACK` guarantee §4.1).
- `RunEntry.run_slug` / `plan_slug` (`orchestrator.rs:730–740`) and the
  supervisor plumbing become `Slug`: the `DriverContext` fields
  (`supervisor.rs:475`, `:498`), the `driver_context` builder params
  (`supervisor.rs:738–746`), and the `run_graph` params
  (`supervisor.rs:837–839`); the task-branch `format!`
  (`supervisor.rs:1738`) and `DriverGuard.plan_slug` (`supervisor.rs:1445`)
  follow. The audit-registry edge (`supervisor.rs:1622–1628`) keeps `String`
  via `to_string()`.
- `RunMetadata.run_slug` (`run_metadata.rs:62–77`) becomes `Slug` (same
  `try_from` serde; existing `run.json` files hold derived slugs and load).

The compiler drives the full list; the rule is the boundary: **a raw `&str`
slug must not survive past a constructor that builds a path or branch name**.

## 0061 — Per-slug run exclusivity

`start_run` (`orchestrator.rs:853`) already does all its decision-making in
one registry critical section (`orchestrator.rs:856–919`). Add the exclusivity
check there, after the blocked-ingestion check (`orchestrator.rs:864–878`) and
before the handle swap (`orchestrator.rs:882–891`):

```rust
// Per-slug exclusivity: the worktree namespace `{plan_slug}--{task_id}` and
// the reclaim heuristic (worktree.rs:209–216) assume at most ONE live run per
// plan slug. Refuse to start while another run on this slug is live.
// (Pending/Completed/Failed peers don't block; resuming THIS run is fine.)
if let Some((other_id, status)) = runs.iter().find_map(|(id, e)| {
    (*id != run.0
        && e.plan_slug == entry_plan_slug
        && matches!(e.status, RunStatus::Running | RunStatus::Paused))
    .then_some((*id, e.status))
}) {
    return Err(ApiError::InvalidCommand {
        reason: format!(
            "another run (run {other_id}) over plan `{entry_plan_slug}` is \
             {status:?}; cancel it or let it finish before starting this one"
        ),
    });
}
```

Implementation note: the snippet needs the immutable scan *before*
`runs.get_mut(&run.0)` (or a re-borrow) to satisfy the borrow checker — do the
scan first, then take the mutable entry. `RunStatus` is
Pending/Running/Paused/Completed/Failed (`api.rs:206–217`); terminal statuses
are stamped at finalization (`orchestrator.rs:481–483`), Paused by `pause_run`
(`orchestrator.rs:994`), so the liveness signal is already maintained.
`ApiError::InvalidCommand` (`api.rs:386–390`) reaches the TUI status bar like
the existing blocked-start reason.

Resume of the *same* run is unaffected (`*id != run.0`), and the defensive
old-handle cancel (`orchestrator.rs:882–884`) is unchanged.

## Test strategy

- `slug_rejects_traversal_and_unsafe_chars` (unit, `task.rs`):
  `../../../tmp/evil`, `a/b`, `a..b`, `A-B`, `a--b`, `-a`, `a-`, `""`, `"a"`
  all rejected; `0005-tui-ingestion-responsiveness-tasks`, `my-feature`, `ab`
  accepted.
- `slug_serde_roundtrips_as_plain_string` (unit, `task.rs`): a `TaskGraph`
  serializes `slug` as a bare JSON string and round-trips; deserializing a
  graph with `"slug": "../evil"` fails.
- `model_supplied_unsafe_slug_is_rejected` (unit, `interpreter.rs`, next to
  the existing `parse_model_response` tests at `interpreter.rs:936` onwards):
  a response with a traversal slug yields `InterpretError::UnsafeSlug` (not
  `ModelResponseInvalid`), and a valid response still parses.
- `create_with_unsafe_plan_slug_is_unrepresentable`: with `&Slug` parameters
  this is a compile-time guarantee; keep the existing task-id tests
  (`tests/worktree.rs:335–399`) green and update call sites
  (`tests/worktree.rs`, `tests/squash_merge.rs` helpers) to construct `Slug`s.
- `second_start_on_same_slug_is_refused_until_terminal` (unit,
  `orchestrator.rs`, NoopBackend harness): open the same task list twice
  (same `plan_slug`), start run A, start run B → `ApiError::InvalidCommand`
  whose reason names the live run; drive A to terminal; start B → succeeds.
- `paused_run_still_blocks_start_on_same_slug`: pause A, start B → refused
  (Paused is live); after `CancelRun` finalizes A as `Failed`, B starts.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- Builds on 0004/0007's ingestion reporting: `UnsafeSlug` surfaces through the
  same interpret-failure → blocking-issue fold (`orchestrator.rs:819–836`), so
  the run stays reviewable-Pending rather than crashing. 0010's persisted
  artifacts already conform to §4.1, so the validating deserialization is
  back-compatible.
- Within this review: independent of plan 0018 (its `merge-staging` slot uses
  a fixed, slug-free name); exclusivity (0061) is what makes the reclaim
  heuristic 0018 leaves in place sound. Plan 0024's gate sandboxing is the
  complementary control for command (not path) injection.
- The `Slug` type is additive for future plans: any new artifact path (plan
  0020's lifecycle work, plan 0023's failure-reason persistence) should take
  `&Slug` rather than `&str`.
