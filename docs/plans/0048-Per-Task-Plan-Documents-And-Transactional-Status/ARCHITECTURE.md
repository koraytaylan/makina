# Architecture — Plan 0048 (deltas)

> Primary new modules: `crates/makina-core/src/plan.rs`, `crates/makina-core/src/plan_status.rs`, `crates/makina-core/src/repository_lease.rs`, and `crates/makina-core/src/landing.rs`; `worktree.rs` gains the private integration workspace.
> Primary existing seams: `orchestrator::{discover_plans, CoreApi::open_run, interpret_and_seed, generate_and_seed}`, `interpreter::*`, `normalizer::*`, `task::{Task, TaskGraph}`, `dependency::EdgeInferrer`, `ingestion::*`, `merge::{MergeOutcome, SquashMerger}`, `actors::supervisor::*`, `run_metadata::*`, `api::*`, plus the binary's project routing, events, application state, TUI, templates, and scaffolding.
> Symbol names are anchors; locate by symbol rather than line number.

## 0001 — Typed plan documents and validation

Today Markdown headings and list conventions are interpreted independently in several places. This workstream creates one closed, typed boundary before scheduling code sees a task.

### One-task bootstrap

This plan changes the format used to execute plans, so it cannot self-host from
the first line. `bootstrap-per-task-plan-execution` is the sole exception: an
external coordinating agent creates a run-qualified private integration
worktree/ref, acquires an exclusive Unix `flock(2)` on the persistent,
never-unlinked `<git-common-dir>/makina.repository.lock`, commits the
coordinator-owned Phase R first from the exact target-base commit that already
contains `SCOPE.md`, `ARCHITECTURE.md`, `STATUS.md`, all 15 validated task
documents. R derives/overlays Plan 0048's row on the base board rather than
requiring or trusting a working-tree row. Working-tree-only source is an explicit
`AwaitingCommit` prerequisite failure. R binds/verifies validation provenance
and carries plan/source/executable-digest/base trailers. The coordinator then
commits the `in-progress` claim in all three
status layers, implements the task there, verifies its changed paths
against the authored footprint, and creates its Phase A commit with
plan/task/run trailers and expected-old ref compare-and-swap—without invoking
Makina's current implement-plan merge/reset path or touching the operator
checkout. The installed per-task `.claude` executor then writes the bootstrap
task's own Phase B frontmatter/STATUS/root bookkeeping from A and executes the
rest of this DAG while holding that same cross-process lease. It enforces the
portable footprint at review and again immediately before every Phase A. At no
point is a temporary `TASKS.md` created, and the user never performs a manual
status transition.

### Document model

Add `serde-saphyr = "0.0.29"` and `sha2 = "0.10.9"` to workspace dependencies
and `makina-core`, retaining `serde` for typed deserialization. The dependency
task verifies both resolved crates against the workspace's Rust 1.85 floor
before the lockfile change is accepted.

`crates/makina-core/src/plan.rs` owns the public source model. Names may adapt to local conventions, but the separation is fixed:

```rust
pub struct PlanKey {
    /// Canonical, repository-relative `docs/plans/<number>-<slug>` path.
    pub relative_dir: PathBuf,
    pub number: String,
    pub slug: String,
}

pub struct PlanDocument {
    pub key: PlanKey,
    pub title: String,
    pub scope: MarkdownDocument,
    pub architecture: MarkdownDocument,
    pub status: PlanStatusDocument,
    pub workstreams: BTreeSet<String>,
    pub tasks: Vec<TaskDocument>,
    pub source_digest: SourceDigest,
    pub executable_digest: PlanDigest,
}

pub struct TaskDocument {
    pub source_path: PathBuf,
    pub sequence: TaskSequence,
    pub frontmatter: TaskFrontmatter,
    pub body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFrontmatter {
    pub id: TaskId,
    pub title: String,
    pub workstream: WorkstreamId,
    pub kind: TaskKind,
    pub depends_on: Vec<TaskId>,
    pub gated: bool,
    pub touches: Vec<RepoPattern>,
    pub status: AuthoredTaskStatus,
    #[serde(with = "empty_string_as_none")]
    pub merged_as: Option<GitObjectId>,
}
```

`TaskId`, `WorkstreamId`, `TaskSequence`, `RepoPattern`, `SourceDigest`, `PlanDigest`, and `GitObjectId` are validated types, not unchecked strings. `RepoPattern` distinguishes ordinary write patterns from exact `TrackedMakinaConfig` and `TrackedMakinaDeletion` exceptions so enforcement cannot lose their modify-only/deletion-only policies after parsing. The empty-string codec maps the authored `merged_as: ""` form to `None` and renders `None` back to that exact form. A non-empty object ID is validated against `git rev-parse --show-object-format` for the repository—40 hex for SHA-1 or 64 for SHA-256—and display layers may shorten it. `TaskKind` is `task | spike | chore`; `AuthoredTaskStatus` is `planned | in-progress | done | blocked | dropped`.

`PlanKey` is equally closed because its folder is also a Git-ref component. The
basename is exactly `NNNN-<slug>`; `<slug>` is ASCII
`[A-Za-z0-9]+(?:-[A-Za-z0-9]+)*`, the complete basename is at most 200 bytes,
and `refs/heads/plan/<basename>` must pass `git check-ref-format`. The ref uses
that basename directly, so the mapping is reversible rather than a lossy
sanitization. This rejects `.lock`, trailing dots, `@{`, spaces/control bytes,
and every Git ref metacharacter during loading rather than later registration.

The YAML boundary is intentionally smaller than general YAML:

- one `---`-delimited frontmatter document at byte zero;
- UTF-8 only, bounded file size, scalar length, collection size, and nesting depth;
- no aliases, anchors, explicit tags, merge keys, directives, or multiple documents;
- duplicate keys rejected before typed deserialization;
- unknown fields rejected by `serde`;
- values rendered in the canonical key order used by this plan, with strings quoted only when YAML requires it;
- Markdown body preserved byte-for-byte when only frontmatter bookkeeping changes.

Parsing consumes a `PlanFileSource` rather than assuming the operator checkout.
Implement a contained filesystem source for authoring/generated directories and
a read-only Git-tree source (blob/tree entries from an exact OID/ref without
checkout) for retained/base views. `load_plan(source, plan_key)` uses identical
parsing/validation for both. The filesystem source never follows a symlink and
rejects repository escape; the Git source rejects symlink/submodule entries
where ordinary plan blobs/directories are required. The same path rules apply
to generated output and status rewrites. The interface also exposes
`is_tracked_ordinary_file(path)` against an explicit, durable validation
provenance. The coordinator binds `validation_base_oid` to the current base
when it registers a plan; it is immutable thereafter and retained by plan Git
history. Every filesystem or Git-tree source resolves trackedness from that
exact commit, never from the source's current tree or the operator's current
index. In authoring mode, a source with blank provenance may parse exact
`.makina/config.toml` as inert `TrackedMakinaConfigCandidate`, and another exact
`kind: chore` `.makina` path only as inert
`TrackedMakinaDeletionCandidate`; either is visible/committable but cannot
execute. Registration supplies the exact base, proves an ordinary tracked blob,
resolves the candidates to modify-only `TrackedMakinaConfig` or deletion-only
`TrackedMakinaDeletion`, binds the OID, and reruns full validation before R; an
untracked/wrong-base path fails closed. Because
completed-plan validation still uses the recorded base, deleting the permitted
artifact does not make the plan invalidate itself afterward.

`PlanStatusDocument` also parses fixed, closed anchors for display status,
progress counts, integration state, base name/OID, final-integration OID,
Exceptions, outcome, and last-updated revision. Integration state is `planned |
assembling | awaiting-integration |
finalization-pending | integration-blocked | complete`; it is durable plan-level
state, not inferred solely from task status. The anchor also stores the current
run UID (when any), mutable expected base name/OID, immutable validation-base
OID, selected final mode, and F OID; `—` is the canonical empty form where a
state permits absence. Registration binds an unregistered plan's validation
base to the then-current base commit; a registered/executable plan requires it,
and later status/finalization rewrites cannot change it. A completion commit
cannot contain its own OID, so C is never a STATUS field; it is recovered from
exact trailers/history and recorded in run metadata only after C exists.

### Bundle validation

A candidate new-format directory is one that contains a `tasks/` directory. It must also contain `SCOPE.md`, `ARCHITECTURE.md`, and `STATUS.md`, must not contain `TASKS.md`, and `tasks/` must contain at least one ordinary `*.md` file and no nested task directories. A directory without `tasks/` is not a candidate even when it contains the historical SCOPE/ARCHITECTURE/STATUS/`TASKS.md` quartet. A directory containing both `tasks/` and `TASKS.md` is a malformed new-format candidate, never a fallback or precedence choice.

Validation is accumulated and deterministically sorted into a `PlanValidationReport`; one bad task must not hide unrelated errors. Each diagnostic carries a stable code, repository-relative path, optional field, and actionable message. Validation covers:

- folder prefix and plan number agree with the H1s in `SCOPE.md`, `ARCHITECTURE.md`, and `STATUS.md`;
- the plan basename is canonical `NNNN-<slug>`, with ASCII alphanumeric tokens
  separated by single hyphens, at most 200 bytes total, and its reversible
  `refs/heads/plan/<basename>` form passes `git check-ref-format` without
  sanitization;
- a new-format candidate's plan number is unique across the union of every
  numbered directory in the plan root (including historical/inert directories)
  and every verified Phase-R registration ref (including ref-only generated
  plans); existing historical↔historical collisions are left untouched and do
  not become executable diagnostics;
- every `- **NNNN — <name>.**` workstream declared under `SCOPE.md`'s In-scope section has exactly one matching `## NNNN — <name>` architecture section, and every task names one of them;
- task filenames match `^(?<ws>\d{2})(?<seq>\d{2})-(?<id>[a-z0-9]+(?:-[a-z0-9]+)*)\.md$`;
- workstreams and task sequences are decimal `0001..0099` and `01..99`;
  filename pair `NN` maps exactly to workstream `00NN` (`01` ↔ `"0001"`),
  making the format's representable bound explicit;
- the filename ID, frontmatter ID, mapped workstream prefix, and per-workstream sequence agree;
- IDs and numeric task prefixes are unique within the plan; dependency IDs resolve within the same plan, contain no self-edge or duplicate, and form a DAG;
- the body H1 exactly matches `title`, contains an ordered `**Steps:**` section, and ends with one non-empty `- **Done when:**` criterion;
- `touches` entries are repository-relative, normalized, non-empty, exclude `.git`, ordinary `.makina` runtime paths, parent traversal, absolute paths, and the active plan status files reserved for the coordinator. In authoring mode blank-provenance exact `.makina/config.toml` becomes a non-executable tracked-config candidate, while any other `kind: chore` exact `.makina` path becomes a deletion candidate. Registration must prove an ordinary blob in the exact validation base and resolve the former to status-exactly-`M` while remaining an ordinary file, and the latter to status-exactly-`D`; every addition, config deletion, artifact modification, rename/copy, type/unmerged/submodule change, glob, or untracked path fails. This permits deliberate committed config maintenance and obsolete-artifact removal without authorizing runtime-state writes or invalidating the completed plan after deletion;
- `done` requires `merged_as`; `planned`, `in-progress`, `blocked`, and `dropped` require an empty `merged_as` until a later policy explicitly permits retained evidence for dropped work;
- plan-status counts match task frontmatter, integration-state/evidence fields are coherent, and every blocked/dropped task has one coordinator-authored Exceptions entry;
- repository-level validation separately checks the root roll-up row against plan number, title, display status, explicit done/dropped/total progress, outcome, and `STATUS.md` link. `load_plan` itself can validate a temporary/unregistered bundle without requiring an external row.

The executable digest is computed from the plan key plus normalized immutable task fields (`id`, `title`, `workstream`, `kind`, ordered dependencies, `gated`, normalized `touches`, and Markdown instructions). It excludes authored status, `merged_as`, plan-status narrative, root roll-up materialization, whitespace that the canonical renderer normalizes, and runtime state.

`SourceDigest` separately covers the closed authored bundle (Scope,
Architecture, task files, and authored STATUS narrative) while normalizing out
coordinator-owned status/progress/integration/Exceptions fields and the derived
root row. It detects authoring edits across R/R2 without changing merely because
the coordinator rebinds validation base or updates status. A coordinator Ungate
therefore changes both SourceDigest and PlanDigest; it is valid only through the
typed, exact-diff disposition chain defined below. The exact Git tree/commit
remains the byte-level evidence for each registered revision.

Both digests use one locked cross-language framing, not JSON serialization or
ambiguous concatenation. The algorithm is SHA-256, lowercase 64-hex output,
with domain prefixes `makina.source-digest.v1\0` and
`makina.executable-digest.v1\0`. Each typed record is encoded as a 4-byte
big-endian tag length, UTF-8 tag, 8-byte big-endian value length, and raw value;
lists first encode an 8-byte count and then framed items. Plan/task paths and
task records sort by normalized repository-path bytes; dependencies retain
canonical declared order; map/set fields sort by canonical UTF-8 bytes.
Markdown uses UTF-8 with CRLF normalized to LF and otherwise byte-preserved;
frontmatter is encoded from typed canonical fields. SourceDigest records
PlanKey/title, complete Scope/Architecture Markdown, authored STATUS
Goal/Root-cause/Approach/Outcome, and every task's immutable fields/body while
substituting canonical values for coordinator-owned fields. PlanDigest uses the
documented executable subset. A committed golden-vector fixture (including
empty/list/non-ASCII/boundary-length cases) must match the bootstrap Python and
Rust implementations; protocol changes require a new version/domain.

## 0002 — Runtime projection

### Source projection and checkpoint reconciliation

`TaskGraph` remains the scheduler's graph, but it is produced only from a validated `PlanDocument`. Extend each task with immutable authored metadata and its repository-relative source path. Preserve a clear boundary:

```text
PlanDocument (durable source)
          │ validate + digest
          ▼
TaskGraph (execution projection) ◀── checked runtime checkpoint
          │
          ▼
Supervisor / agents / integration coordinator
```

`CoreApi::open_plan` is read-only with respect to Git/source/runtime state and
performs these steps in order:

1. Load and validate the selected plan directory (or its retained integration-ref view on resume).
2. Build the source graph and compute its executable digest.
3. Read runtime JSON if present.
4. Overlay only volatile runtime fields when plan key, task IDs, and digest agree.
5. Inspect authored status, Git evidence, active worktrees, and checkpoint into
   a `ReconciliationPlan` without changing any of them.
6. Expose the open/preview graph and whether start requires recovery, but do not
   call it running or persist a normalized checkpoint yet.

`start_plan` first acquires the repository lease, then rereads all source, Git,
worktree, and checkpoint evidence to close the open→start TOCTOU window. Only
that lease-bound apply pass may repair status, normalize a checkpoint, create a
plan/integration ref, or emit `Running`.

A mismatched checkpoint is not silently preferred. It is archived or rejected with an actionable diagnostic according to whether it contains recoverable active-work references; the source graph remains authoritative. `Done` is never restored from JSON alone.

Checkpoint files no longer live under the repository's committed `.makina`
tree. Make `paths::state_root` (and its derived helpers) fallible and store
checkpoints at a plan-qualified path below
`paths::state_root(repo_root)?.join("checkpoints")` (with a safe encoded/hash
component for the complete `PlanKey`). The resolver must return an error when a
user state directory is unavailable, cannot be secured, or resolves inside the
repository; remove the current HOME-less
`repo_root/.makina` fallback for mutable runtime artifacts and update all
callers. Read-only open reports an unavailable checkpoint location without
mutating; start cannot create refs/worktrees or run agents until an external
state root is available. No compatibility read/migration of repository-local
checkpoint JSON is needed.

The deterministic `StructuredTextInterpreter`, its `TASKS.md` linter, the preview-only `parse_plan_tasks`, and synthetic task-list source paths are removed. `ModelInterpreter` may help create a plan bundle, but model output crosses the same typed loader and cannot inject a second runtime schema.

### Authored scheduling semantics

Projection and scheduling follow these rules:

- `planned` becomes schedulable when dependencies are satisfied and `gated` is false;
- `in-progress` is resumable only when a matching live driver exists; otherwise lease-bound reconciliation either completes a proven Phase A landing or returns it to `planned` only after proving that no dirty/divergent task worktree or unlanded branch evidence would be lost;
- `done` seeds a terminal runtime state only after `merged_as` resolves to a matching task landing commit on the retained plan ref;
- `blocked` is terminal for that run until an explicit retry moves it to `planned`; downstream tasks remain visibly waiting rather than being mislabeled complete;
- `dropped` is terminal and never dispatched, but it does not satisfy `depends_on`; after the first claim/run/disposition evidence freezes the registered source, a dependent remains blocked until it too is dropped or moved into a newly authored plan;
- `gated: true` remains visible as planned but is never dispatched automatically. Before any claim/run/disposition evidence the author may commit an edit and replace R through verified R2; after that freeze boundary only the lease-bound coordinator disposition command may ungate or drop it.

`touches` uses one documented portable grammar: normalized literal paths,
`*` within one segment, and terminal `/**` recursion; no `?`, character classes,
braces, extglobs, negation, or platform-specific separators. It becomes
`EdgeInferrer`'s primary collision signal. Exact path overlap, parent/child
overlap, and intersecting supported globs create a deterministic serialization
edge unless the author already supplied an ordering dependency. First compute a
deterministic topological linear extension of the authored DAG (Kahn-ready ties
break by numeric/source order); orient every collision between incomparable
tasks from earlier to later in that one order, then assert the augmented graph
is acyclic. Pairwise local preferences are forbidden because they can form a
cycle when an authored edge opposes numeric order. Textual
backtick/path inference is retained only as a diagnostic supplement for missing
or suspicious footprints and cannot remove an explicit collision.

Before review acceptance and again immediately before Phase A, compute the
task branch diff from its recorded base, including both sides of renames and
submodule entries, and require every changed path to match `touches`. Reserved
status paths always fail. An undeclared path returns the task to correction.
Before the first claim, a genuinely broader intended footprint requires a
committed author edit plus explicit R2 refresh; after claim, broader work moves
to a newly authored plan because no footprint edit is legal. It is never
silently landed. This
makes footprints enforceable scheduling claims, though it still does not
sandbox an agent from Git/common-directory tampering.
For either tracked `.makina` variant, parse a NUL-safe name-status diff with
rename/copy detection. `TrackedMakinaConfig` accepts only exact
`.makina/config.toml` with status `M` and an ordinary-file result;
`TrackedMakinaDeletion` accepts only its single exact path with status `D`.
Every other status (`A`, wrong `M`/`D`, `R*`, `C*`, `T`, unmerged, or submodule)
fails even though the path text matches.

## 0003 — Plan-directory identity and UI

### Core and persisted identity

Replace `task_list_path` with `plan_dir` throughout `api.rs`, `orchestrator.rs`, `run_metadata.rs`, and event payloads. Since there is no compatibility requirement, serialized fields and command variants may be renamed directly:

- `Command::OpenRun { task_list_path }` → `Command::OpenPlan { plan_dir }`;
- `RunView.task_list_path` → `RunView.plan_dir`;
- `RunEntry`, `RunOpened`, reset/replay metadata, and task-entry sources store the same canonical repository-relative `PlanKey`;
- run slug derives from the whole plan folder name, while persistent `run_uid` continues to distinguish attempts;
- runtime checkpoint paths are plan-qualified caches below the fallible,
  off-repository per-project `state_root/checkpoints` directory and are keyed by
  `PlanKey` + digest rather than source identities; no runtime fallback writes
  beneath repository-local `.makina`.

`discover_plans` becomes the only plan scanner. It takes the union of numbered
working/base candidates and `plan/*` refs, derives/validates `PlanKey` from R
trailers/tree rather than requiring a checkout path, and correlates the views.
It returns:

- a ready/not-yet-started entry when R is the plan-ref tip, or an active/retained
  entry when exactly one verified R is an ancestor on the expected first-parent
  plan lineage and the current tip is classified by claim/A/B/P evidence; the
  imported root row/provenance/digests must agree in either case;
- an `AwaitingCommit` entry for a valid working-tree candidate whose exact
  closed bundle is absent from current target base, and an `Unregistered` entry for
  committed source that lacks R; both are diagnosable but non-executable;
- an invalid entry with the shared validation report for a candidate new-format directory;
- nothing for historical directories without `tasks/` and no R, or unrelated
  folders. An R-only generated plan with no operator/base directory remains
  discoverable across restart.

Discovery, direct open, reset, resume, history reconstruction, registration, and generation all call the same source-parameterized loader. Filesystem discovery uses the working-tree source for candidates; ready/active/history views use the exact R/plan/base Git-tree source. There is no independent fast preview parser. Empty/invalid `tasks/` candidates, source-digest divergence, and unregistered bundles remain visible with diagnostics/actions so authors can fix/recover them.

### Binary routing and presentation

Retarget `project_api.rs`, `event.rs`, `app.rs`, and `ui.rs` to `PlanKey`/`plan_dir`. Preserve repository containment and symlink rejection when routing a plan from a multi-folder project. Remove `PlanIdentity` fallbacks that derive identity from `TASKS.md` or its parent.

The TUI renders typed data:

- plan title, aggregate status, progress, outcome, and validation state from `PlanDocument`;
- task ID/title, workstream, kind, gate, authored status, dependency state, footprint, and abbreviated `merged_as` from `TaskDocument`;
- Markdown preview bodies from the loaded documents, without heading/list heuristics;
- collision edges separately from authored dependencies;
- a clear historical/inert state only where a user navigates directly to an old plan path—historical plans do not appear as executable runs.

For an active or non-finalized plan, the view resolves source documents from the retained `plan/{slug}` ref/integration view before consulting the base checkout. This makes coordinator status commits visible without checking the operator's branch out. After successful finalization, the ordinary base view contains the final documents. Refresh events are emitted only after the status transaction commits.

## 0004 — Transactional status and Git evidence

### Repository execution lease

Add a repository-run lease keyed by the canonical Git common directory, not the current worktree path. One registry is injected by the binary/project-router composition root into every `CoreApi` so two project APIs or worktree aliases in the same process cannot obtain independent locks; tests inject isolated registries. Pair its fair in-process queue with `<git-common-dir>/makina.repository.lock`. On Unix both the bootstrap Python holder and Rust use an exclusive whole-file `flock(2)` on that exact file, opened without truncation and never unlinked or inode-replaced; close releases ownership, including after a crash. The Rust backend may use the platform-equivalent primitive elsewhere, while the bootstrap fails closed where the canonical protocol is unavailable. A subprocess conformance test holds the Python lock while Rust contends and then proves crash release.

Crash release is delayed until every mutating child is quiescent. On Unix the
lease owner spawns coordinator Git children with a controlled inherited
duplicate of the same locked open-file description (not a second pathname open)
so SIGKILL of the parent cannot release the repository lock while an orphan Git
process still mutates. The duplicate is absent from agents and non-mutating
children and closes on Git exit; normal cancellation still terminates/reaps the
child before releasing the parent's copy. The bootstrap Python holder brokers
mutating Git children for the same reason. Hard-kill tests pause a Git child,
kill its coordinator, and prove a contender cannot acquire until that child
exits. Platforms unable to provide an equivalent child-lifetime guarantee fail
closed for execution.

Opening and reconciliation inspection are concurrent/read-only. Starting acquires the lease before emitting `Running`, creating/checking out any plan ref/workspace, or mutating status; it then rereads all evidence before applying reconciliation. A second run in the repository stays queued with its owner/reason visible, while different repositories remain concurrent.

Every repository-mutating maintenance API uses this same lease. In particular,
`Command::PurgeWorktrees` must acquire it (or return the same visible busy
state) before pruning/removing registrations or paths; it may not race an
active plan session. Purge applies the same ownership, clean/untracked,
reachability, and recovery-ref checks as ordinary task/integration cleanup and
preserves any active, dirty, divergent, or ambiguous workspace.

The transaction coordinator—not a cancellable task driver—owns the repository lease and the inner integration lock. Developer/reviewer work runs outside the short inner lock, but claim/status commits, task landings, blocker/retry commits, and finalization use it. Once a Git phase begins, cancellation is deferred until its child has exited/reaped and state reaches a reconciliable boundary. Stage/Manual, or automatic Squash/MergeCommit blocked after durable P because the base is checked out, may release the lease only after the retained ref/workspace and `finalization-pending` status are durable and every child is quiescent. Their typed later finalization reacquires the lease and rereads all evidence; the original session never waits while holding the lock for a command that must reacquire it.

### Private integration workspace

Create one run-qualified, Makina-owned integration worktree at
`paths::run_dir(repo_root, run_uid)?.join("integration")`, beneath
the existing per-project state root (normally
`~/.makina/projects/<project-namespace>/runs/<run_uid>/integration`). Never use
a repository-local `.makina/runs` literal for a worktree. Claim/status commits,
registration R, task phases A/B, final phases P/F/C, conflict handling, and Stage operate only
there. The operator checkout is never switched, staged, merged, reset, cleaned,
or used as the transaction index.

Registration starts this workspace detached at the expected base with a unique
operation identity. It must not use `git worktree add -b` or otherwise expose
`plan/{slug}` before R is complete. Build and validate R while detached, then
publish only with one `git update-ref --stdin` transaction that verifies the
target base ref still equals `expected_base_oid` and creates
`refs/heads/plan/{slug}` at R from the zero OID. An allowed pre-claim R2 refresh
uses one transaction to verify the base and expected old R, create-or-verify the
deterministic archive ref, and advance the plan ref. Only after the transaction
commits may the workspace attach to the plan ref. A crash before CAS leaves the candidate
reachable from the owned detached workspace/reflog for inspection; recovery
verifies/reuses or preserves it and never invents an empty/non-R plan ref.

Creation is create-only. Existing worktree registrations, paths, refs, index
locks, dirty files, untracked files, or divergent branches are inspected as
recovery evidence; nothing is force-removed. Every shared ref update supplies
the expected old OID and fails closed if the ref it advances moved. R/R2 and P
publication atomically verify the current target-base ref because each imports
its board/tree snapshot; F/C CAS that base directly. Mid-run claim, B,
blocker/retry, and disposition commits CAS only the plan ref and retain their
recorded base snapshot, so unrelated base movement cannot deadlock task
bookkeeping; P later reconciles the latest base/root board without rewriting
prior evidence. Git subprocesses are
configured to terminate and be reaped on cancellation; an unclear child/index
state preserves the workspace and blocks instead of attempting destructive
cleanup. Stage leaves its index/diff in this private durable workspace. Manual
leaves a clean plan ref and exposes the typed delayed-finalization command.

Before Phase F or C advances the target base ref, inspect `git worktree list`
and refuse when that branch is checked out in any non-Makina worktree. P may be
prepared in a detached Makina workspace because it verifies but does not move
the target base. Updating
the ref behind an operator index would make the checkout logically inconsistent
even without changing its bytes. Automated finalization may use a Makina-owned
clean base/finalization worktree only when the branch is otherwise free; if it
is not, persist `awaiting-integration` and require an explicit later finalize
after the operator releases the branch.

### Registration and source import

A working-tree bundle or root row is authoring input, not executable truth.
Filesystem discovery reports `AwaitingCommit` until the exact closed plan
bundle exists in the current target-base commit. A pre-existing base row is
validated if present but is not required; R derives/inserts it. `Command::RegisterPlan {
plan_dir, expected_base_oid, expected_source_digest }` acquires the repository
lease, rereads that base commit, numbered namespace, plan, and board through the
Git-tree loader, and fails if the committed bytes differ from the inspected
digest. In the private integration workspace the coordinator verifies/binds
`validation_base_oid`, derives this plan's row, overlays only that row on the
exact base board when needed, reloads both layers, and commits Phase R on a
create-only `plan/{slug}` ref:

Root-row cardinality is closed: zero matching-number rows inserts the derived
row; one byte/field-exact semantic row is reused; one mismatched row returns a
diagnostic/proposed diff without rewriting authored base history; and more than
one matching row fails as ambiguous. Generated-origin registration always
starts from the current base board and follows the same rule.

```text
Makina-Phase: plan-registration
Makina-Plan: <plan-folder-slug>
Makina-Source-Digest: <normalized closed authored-bundle SourceDigest>
Makina-Executable-Digest: <PlanDigest>
Makina-Validation-Base: <expected-base-oid>
Makina-Source-Origin: base | generated
```

The Git commit/tree is the atomic multi-file boundary. No operator index, ref,
or existing file is changed. Zero matching R evidence permits detached
candidate creation followed by one base-verify/create-plan-ref transaction, one exact matching tip
is verified and reused after response loss,
and an existing non-R/divergent/off-lineage ref blocks. If target base or the
committed plan source changes before any claim/run/disposition evidence, the same command
may refresh registration with expected-old-R CAS: first retain old R under
`refs/makina/recovery/registration/<plan>/<R>`, then create R2 from the current
committed source with `Makina-Previous-Registration: <R>` and move the plan ref.
The R2 archive/create-or-verify plus plan-ref move and current-base verification
form one idempotent ref transaction; response loss can observe either no ref
change or the complete verified update, never an unarchived moved registration.
For a generated-origin R whose bundle exists only in R, R2 replays the exact
verified closed plan subtree from R onto the new base, rejects any new
number/path conflict, revalidates tracked-config/deletion candidates and validation
base against that base, and rebuilds only its root row. It never requires an
operator copy or silently changes the executable/source digest.
After any claim/run/disposition evidence exists, refresh is forbidden. Before
that freeze boundary, `start_plan`
requires an exact current R and rereads it through the Git-tree loader; a bundle
committed on base still receives R (possibly with the same tree) so registration
has one invariant. Uncommitted edits between open, register, or start are
reported as working-source divergence but cannot enter R or the run. After
execution starts, only supported coordinator mutations may change source.

Product generation is the only non-base input to R: it renders into an
off-repository temporary directory controlled by Makina, validates there, then
under the same lease copies that closed bundle into the private workspace and
creates R directly. It never publishes untracked plan paths in the operator
checkout. A failed pre-R generation removes only its owned temporary; R success
is durable publication and response-loss recovery finds it by exact evidence.

### Claim and landing lifecycle

The coordinator, not the developer agent, mutates durable status:

```text
planned
   │ coordinator claims under integration lock
   ▼
in-progress ── agent/reviewer/fixer work outside the lock
   │
   ├── durable failure ──► blocked + status bookkeeping commit
   ├── cancel/requeue ───► planned + status bookkeeping commit
   └── accepted work
          │ Phase A: squash task changes onto plan/{slug}
          │          + provenance trailers; capture full OID A
          ▼
      in-progress, landing-pending
          │ Phase B: render task done/OID A + plan/root status
          │          validate all layers + bookkeeping commit B
          ▼
        done ── then persist runtime Done and remove worktree
```

At claim, the coordinator commits the `in-progress` transition before the task branch/worktree is cut, so every worker sees the current plan source. Role prompts reserve the active plan's `tasks/*.md`, its `STATUS.md`, and root `docs/plans/STATUS.md`; a worker edit to those paths fails changed-path validation rather than entering the task landing.

Extend merge results so a successful task landing returns a validated full OID:

```rust
pub enum MergeOutcome {
    Merged { implementation_commit: GitObjectId },
    Conflict { /* existing evidence */ },
    // existing non-success outcomes
}
```

Phase A's commit message carries:

```text
Makina-Plan: <plan-folder-slug>
Makina-Task: <plan-local-task-id>
Makina-Run: <run_uid>
```

Before creating A, the coordinator searches the expected first-parent plan
lineage for the exact plan/task/run tuple. Zero matches permits the squash, one
matching commit is verified and reused, and multiple/off-lineage matches block
as ambiguous. After checkpoint loss, `Makina-Run` is recovered from the commit:
plan/task/lineage are always required, and run identity is compared when matching
durable run metadata exists rather than being invented from a new attempt.

Phase B uses `plan_status.rs` and `landing.rs` to update only coordinator-owned
documents in the private integration workspace. It edits this plan's row in the
recorded board snapshot already carried by the plan lineage and CASes only the
expected plan ref; unrelated target-base movement does not strand bookkeeping.
P later rereads the current base board and overlays the latest row before any
integration. The writer captures original bytes/index entries, writes
replacements atomically, reparses the entire plan, verifies root roll-up
consistency, and commits a separate bookkeeping change with
`Makina-Phase: task-status` and `Makina-Landing: <A>` trailers. It never mutates
the operator checkout.

If Phase B fails:

- on a handled error, restore only partially written coordinator-owned integration-worktree files/index entries to Phase A with path-limited operations; on a crash/ambiguous index state, preserve the whole private workspace for startup reconciliation;
- leave implementation commit A on `plan/{slug}`;
- keep task runtime non-done (`InReview`/landing-pending), and retain its branch/worktree;
- report a recoverable integration diagnostic containing A's full OID;
- on resume, find A by its exact trailers and finish Phase B without re-squashing task changes.

The run pauses further claims and integration mutations after an incomplete
Phase B until reconciliation succeeds. Already-running agents may finish and
retain their worktrees, but their results are not landed past an unresolved
bookkeeping boundary.

`merged_as` is A, never B. Runtime `Done`, completion events, and worktree removal occur only after B exists and the freshly loaded source agrees.

Every B commit is found/reused by its phase/linkage trailers before another is
created. Task/integration refs advance with expected-old-OID compare-and-swap,
so an external process or delayed run cannot silently overwrite evidence.

After the first claim/run/disposition evidence, authored dependencies and
instructions are frozen for that registered revision. The
only scheduling-source mutation is
`Command::SetTaskDisposition { run, task, expected_plan_oid, action }`, where
`action` is `Ungate` or `Drop { reason }`. Under the repository lease it permits
Ungate only for a planned gated task and Drop only when no live driver, dirty/
divergent worktree, Phase A, or unlanded branch evidence exists. It renders task
frontmatter plus Exceptions/plan/root status in the private workspace,
validates, and commits with `Makina-Phase: task-disposition`, plan/run/task,
previous/new SourceDigest, previous/new PlanDigest, and action trailers. Ungate
authorizes exactly `gated: true -> false` plus derived coordinator fields; Drop
changes only status/Exceptions/roll-up material, so its old/new digests are
equal. Active discovery and reconciliation derive the current authorized
digests by folding this exact first-parent disposition chain from R and reject
any unexplained post-R source change. Only after the CAS commit does runtime
invalidate/rebuild the graph/checkpoint when PlanDigest changed. A crash between
commit and runtime rebuild reconciles from source; runtime-only disposition is rejected.
Dropped dependencies remain unsatisfied, so downstream work stays visibly
blocked and may be dropped explicitly but is never silently rewritten.

### Status derivation

`PlanStatusDocument` recognizes the fixed heading, `Status`, `Goal`, `Root cause`, `Approach`, `Progress`, `Integration`, `Exceptions`, `Outcome`, and last-updated anchors used by this plan. `Integration` stores typed state, current run UID, expected base name/OID, selected final mode, and F OID. It never stores C's self-referential OID. `Exceptions` is a deterministic coordinator-owned, append-only-in-plan-history list of task/status/reason/timestamp records for blocked or dropped work; retry/resolution marks the record resolved rather than erasing it. Reasons are bounded, single-line, and Markdown-escaped. The renderer preserves Goal/Root cause/Approach prose and changes only owned fields.

Every durable task transition recomputes:

- done, blocked, dropped, in-progress, and total counts from frontmatter;
- the display aggregate using integration-state precedence first, then task
  states (`integration-blocked` beats task progress; `finalization-pending`,
  `awaiting-integration`, and `complete` are never inferred from task counts);
- the one root roll-up row: `done/total` when no tasks are dropped, or
  `done + dropped / total` when they are, so terminal omissions stay visible;
- the plan/status link and outcome text.

Display precedence is exact:

| Integration state | Root/heading display |
|---|---|
| `complete` | ✅ Complete |
| `integration-blocked` | ⛔ Blocked |
| `finalization-pending` | 🔄 Finalizing |
| `awaiting-integration` | ⏳ Awaiting integration |
| `assembling` | 🚧 In progress, unless the scheduler has no active/runnable task and unfinished work is blocked, then ⛔ Blocked |
| `planned` | 📋 Planned |

Finalization is eligible only when every non-dropped task is exactly `done`, no task is planned/in-progress/blocked, no non-dropped task remains gated, and every dropped task has an Exceptions reason. The integration ref first becomes `awaiting-integration`; it is not yet complete. For every configured mode, preparation constructs a new P child of the retained plan tip whose tree is the three-way, conflict-checked integration of that immutable history with the current base/root board. It never rebases or rewrites R/A/B/disposition commits or any `merged_as`; old evidence remains reachable. A conflict sets `integration-blocked` without falsely changing a task status.

P begins the recoverable protocol for `Squash`, `MergeCommit`, `Stage`, and
`Manual`. After eligibility, gates, and expected base/ref checks pass, Phase P
commits `finalization-pending`, current run, expected base, selected mode, and a
blank F field to `plan/{slug}`. It overlays only this plan's Finalizing root row
on the current base board and carries `Makina-Phase: finalization-prepared`,
`Makina-Final-Mode`, `Makina-Plan`, `Makina-Run`, and
`Makina-Expected-Base` trailers. P does not attempt to record its own OID. Its
plan-ref publication atomically verifies the expected base ref, but a checked-
out base blocks only F/C, not preparation that does not advance it.

For `Stage`, preparation also applies the exact expected-base→P integration
tree to the index of a retained, detached Makina finalization workspace and
records its index tree OID; it never stages the operator index. For `Manual`, P
is the exact tree/parent/provenance recipe surfaced to the user. Both modes may
release the lease in durable `finalization-pending` state with P and their
workspace/instructions retained; neither is complete.

Phase F integrates that exact P tip into the expected base and captures the
resulting OID with `Makina-Phase: final-integration`, `Makina-Final-Mode`,
`Makina-Plan`, `Makina-Run`, and `Makina-Plan-Tip: <P>` evidence. Thus the base
tree already says `finalization-pending` if execution stops immediately after
F; F does not attempt to put its own OID in its tree. `Squash` creates a
single-parent F from the prepared tree; `MergeCommit` creates a two-parent F
whose second parent is P. `PreparedStage` first verifies the retained index and
worktree are unchanged and their tree equals the recorded prepared tree, then
creates the same single-parent F from that index. `ManualCommit` accepts only a
full OID already at the target-base tip with first parent equal to P's expected
base, exact prepared tree, exact F trailers, and either no second parent
(squash shape) or P as its sole second parent (merge shape). Phase C then writes
`complete`, the now-known F OID, and the current complete root roll-up in a
separate base commit carrying `Makina-Phase: completion`, `Makina-Plan`,
`Makina-Run`, and `Makina-Final-Commit: <F>`. STATUS has no C field. After C is
created, its OID is persisted to run metadata; after checkpoint loss it is
derived from the exact completion trailers and expected first-parent base
lineage. A crash after P resumes/reuses F, and a crash after F resumes/reuses C,
without repeating an earlier phase. Before F the TUI overlays P from the plan
ref; after F it reads pending/completion state from base.

### Resume and final provenance

Read-only open inspects, and lease-bound start/resume rereads then applies, three evidence layers in this order: current plan documents from the integration/base view, Git history/trailers plus task/integration worktrees, then the matching runtime checkpoint. The reconciler is idempotent:

- `done` + reachable `merged_as` + matching plan/task/lineage landing evidence stays done (run is additionally checked when durable metadata exists);
- Phase A evidence with `in-progress` completes Phase B;
- `in-progress` without a live driver or Phase A returns to planned only after
  proving its task branch/worktree contains no dirty, untracked, divergent, or
  unlanded evidence; otherwise it becomes a recovery blocker and is retained;
- JSON `Done` without source and Git evidence is rejected, never promoted;
- source `done` with a missing/mismatched OID is a validation blocker, never silently rerun;
- phase R/A/B/P/F/C commits are found by exact trailers and expected lineage before
  creation; zero permits creation, one is reused, and multiple block;
- repeated reconciliation creates no duplicate status, landing, final
  integration, or completion commit.

For final `MergeCommit`, task implementation OIDs remain ancestors and F carries the same final phase/plan/run/expected-tip evidence as Squash. For final `Squash` or approved `Stage`, the squash-shaped F additionally carries one deterministic `Makina-Task: <id> <implementation-oid>` trailer per done task; the successful plan ref is retained so those OIDs remain reachable. STATUS records F only when C renders `complete`. Run metadata records F and, strictly after C is committed, C; exact completion trailers/history recover C if that checkpoint is lost. Neither final OID replaces each task's `merged_as`.

Stage/Manual and an automatic mode retained because the base is unavailable
record the base OID they were prepared against, retain their private
workspace/ref, and release the lease only in durable
`finalization-pending` state. The exact delayed entry point is
`Command::FinalizePlan { plan_dir, run_uid, expected_plan_oid, input }`, where
`FinalizeInput` is `Automatic`, `PreparedStage`, or
`ManualCommit(GitObjectId)`. The configured mode must match the input;
`Automatic` serves Squash/MergeCommit retry after an unavailable base,
`PreparedStage` is explicit approval of the inspected private index, and
`ManualCommit` verifies the user landing described above. The command
reacquires the lease, rereads all evidence, reruns gates, and blocks on any
worktree/ref/tree mismatch. If base advanced after any prepared P, every mode
preserves that evidence and requires
`Command::ReprepareFinalization { plan_dir, run_uid, expected_plan_oid }` to
create a new P child/tree from the current base without rewriting the previous P
or any R/A/B/disposition history; every `merged_as` remains unchanged and
reachable. Stage/Manual additionally archive the exact inspected workspace/UI
recipe. It never silently changes bytes the user inspected or lands a stale
root board.

`ResetRun` becomes evidence-aware. With no durable or dirty task/landing
evidence it may discard a verified-clean attempt. Otherwise it either refuses
reset with recovery paths or atomically archives the plan/task refs under a
run-qualified `refs/makina/recovery/...` namespace before starting a new
attempt. It never force-removes a dirty worktree or deletes the only ref that
makes R, `merged_as`, A, B, P, F, or C reachable. A `complete` plan cannot be reset
into a duplicate integration; further work requires a newly authored plan.

## 0005 — Authoring cutover

### Atomic bundle generation and registration

After the repository lease and shared root editor exist, replace
`write_tasks_md`/`render_tasks_md` and monolithic normalizer output with a
`GeneratedPlanBundle`. Whether invoked from the TUI or an authoring workflow,
generation follows this order:

1. choose a proposed plan identity and render `SCOPE.md`, `ARCHITECTURE.md`,
   `STATUS.md`, and `tasks/*.md` into an exclusively created run-qualified
   temporary directory below the external state root;
2. normalize through typed renderers and run structural `load_plan` validation
   outside the lease, with validation provenance still unbound;
3. acquire the repository lease, then reread target base, every numbered
   directory/registered plan ref, and the base root board. Recheck the global
   number/ref namespace under that lease;
4. copy only the closed validated bundle into the private integration
   workspace, bind the immutable validation-base OID, derive/overlay its row on
   the exact base board, validate the complete Git-tree candidate, and create
   Phase R with one target-base-verify/create-plan-ref transaction;
5. return `Registered` and refresh committed-ref discovery when exact R
   verifies, including after response loss.
   On pre-R failure remove only the owned external temporary and return all
   diagnostics; on R success never delete the only ref carrying the generated
   source.

Generation never publishes files in the operator checkout, follows a symlink,
interprets an old `TASKS.md` directory as an empty target, or starts a run.
Hand-authored working-tree candidates instead remain `AwaitingCommit`; once
their exact source lands in target base, the same `RegisterPlan` command creates
R without a duplicate parser or source-import policy.
A generated R whose base advances before claim is non-runnable and exposes
`RefreshRegistration`; the generated-origin R2 path replays/verifies its exact
R subtree onto the new base and returns to `Ready` without an operator copy.

### Skills and workflows

Port `.claude/skills/create-plan`, `.claude/skills/implement-plan`, `.claude/workflows/create-plan.js`, `.claude/workflows/implement-plan.js`, their tests, and agent prompts to the new bundle. The workflows use the same contract as Rust:

- plan-local task identity and dependency DAG;
- strict frontmatter fields and canonical filenames;
- workstream parity across scope, architecture, and tasks;
- developer agents forbidden from status paths;
- serialized coordinator status transitions after Git outcomes;
- full landing OIDs, not guessed/manual SHAs;
- gated and dropped semantics matching runtime behavior.

This task replaces the bootstrap parser/writer after the Rust source, status,
isolation, and reconciliation types exist and ports create-plan. Expose a
versioned, long-lived JSON-lines `plan-contract` session from the Rust
binary/core for inspect/render, candidate diff checking, registration, claim,
landing, blocker/retry/disposition, resume, and finalization. One execution
workflow starts `plan-contract serve` on an authenticated, run-owned local
control endpoint below the external state root (Unix-domain socket or platform
named-pipe equivalent) and connects to it; closed stdio pipes are not treated as
reconnectable transport. `StartSession` validates its plan/run identity,
acquires the canonical repository lease, rereads/reconciles durable evidence,
and returns `Ready { session_token, plan_oid }` only while it owns that lease.
Every mutating request carries the opaque session token plus a monotonically
increasing request ID and expected source/ref OIDs; stale, replayed, unknown, or
cross-session requests fail closed. The process holds the lease while
JavaScript dispatches agents and until `Close` reaches a stable final/retained
state. EOF, parent death, cancel, or protocol failure stops new work, reaps any
Git child, persists/preserves a reconciliable state, and only then releases the
lease. Before each host-level `agent()` call, JavaScript must obtain a typed
`BeginWorker` handle and settle it with `EndWorker` only after that exact Promise
has terminated; a worker-lifetime sentinel holds an inherited lease duplicate.
The Rust server does not pretend bookkeeping is cancellation authority over the
host call. On client death, a live handle leaves the reconnectable session
visibly recovery-blocked with the lease held; it may release only after the
original client reports termination or a recovery client supplies verifiable
host cancellation/termination evidence. If the server itself is hard-killed,
the sentinel keeps the kernel lock until the same verified recovery rather than
allowing a second run to overlap it. JavaScript may orchestrate agents and dependency-ready concurrency, but
every plan parse/render/validation and every coordinator-owned source/Git
mutation delegates to this one session. It must not retain an independent
YAML/Markdown parser, footprint matcher, status renderer, lock, or Git phase
implementation. Contract tests invoke the real binary/core, hold a session idle
while agents would run to prove another Makina process remains blocked, and
reject protocol-version/unknown-field/session-token drift. They also kill the
JavaScript client and the Rust session with a worker active and prove the
lease/recovery barrier survives until that lifetime is resolved.

Plan 0048 itself uses an explicit self-upgrade seam after task 0502's Phase B.
The authoring/scaffold dependency barrier keeps task 0503 from dispatching in
parallel. The already-running bootstrap coordinator stops dispatch, proves all
workflow-owned agents and Git children quiescent or durably retained, persists
the last durable state, and builds the new binary from the exact 0502-B
plan-ref/integration tree into a run-owned external target directory. It records
the artifact hash and embeds B as the build-source OID; it never selects an
installed/PATH or current-base binary. It then closes and reaps its Python lease
holder and launches that absolute binary. The Rust `Hello` must report the
expected protocol and build-source OID before `StartSession`; the session then
reacquires the same lock, rereads R/A/B plus checkpoint/worktree evidence, and
proves resume. The bootstrap copy, exact-B binary/hash/build manifest, and
control-endpoint recovery data remain until verified C plus stable Close; a
retained Stage/Manual plan retains them as well. If
another process wins the close/reacquire gap, the old coordinator reports a
visible wait and performs no mutation; the winner's durable state is reconciled
when acquisition succeeds. Tests cover an intentionally old PATH binary,
handoff success, crash on both sides of release, response loss, a live-worker
barrier, and a contender winning the gap.

The cleanup mechanism must already exist in the exact-B server and future
workflow client; Plan 0048's running bootstrap has the matching v1 client/
janitor prebuilt by task 0101. A path-contained hashed retention manifest is
durable before handoff. Only
verified C plus zero live worker/Git children lets `Close` emit a manifest-bound
`CleanupPermit` and exit; its client reaps the server before removing the exact
owned artifacts, and next startup can idempotently finish cleanup after response
loss. No permit is issued for Stage/Manual retention or ambiguous evidence.

### Normative docs and scaffolds

Create `docs/plans/README.md` as the concise authoring law, using this plan as its linked complete example. Update `docs/spec/structured-text-convention.md`, runtime artifact documentation, `crates/makina/src/templates/plans_readme.md`, scaffold/folder initialization, and live README/help text. New todo scaffolds commit a sample valid bundle on `develop`, register exact R through the shared transaction, then leave a create-only user `workspace` branch checked out at that commit so the sample is Ready and automated F/C never advances a checked-out base. They never create `TASKS.md`.

Historical plan folders remain unchanged. Normative documentation distinguishes them as pre-cutover records, and tests explicitly prove that they are inert rather than malformed candidates.

### Clean removal gate

After all producers and consumers move, remove old parser/normalizer/rendering code, obsolete test fixtures, and live symbols such as `task_list_path`, `is_plan_tasks_path`, `parse_plan_tasks`, `write_tasks_md`, and `render_tasks_md`. A repository search gate allows `TASKS.md` only in:

- completed historical plan content;
- a migration/cutover explanation that explicitly says it is inert;
- dated historical review evidence under `docs/reviews/**`;
- negative tests proving old plans are ignored.

No live code path, scaffold, prompt, workflow, or normative example may create, discover, open, or execute it.

## Test strategy

Each task adds focused tests, and the final cutover runs all repository gates. Required suites include:

- parser fixtures for valid frontmatter, every forbidden YAML feature, duplicate/unknown fields, size/depth budgets, canonical rendering, and body preservation;
- bundle fixtures for `tasks/` candidacy versus historical quartets, explicit
  mixed-format rejection, workstream parity, filenames, ID uniqueness,
  dependency resolution/cycles, footprint safety, symlink/containment attacks,
  source-snapshot trackedness, status/integration/OID coherence, unregistered
  root state, and deterministic multi-error ordering;
- source/checkpoint tests proving executable edits invalidate a checkpoint while bookkeeping edits do not, and JSON-only `Done` never wins;
- scheduler tests for gated, dropped, blocked, resumed, and footprint-collision behavior;
- discovery/API/UI tests proving one plan-directory identity across valid, invalid, active-ref, completed, and historical-only plans;
- concurrency tests proving one injected registry covers all Core APIs, the
  advisory lock covers a second process, and different repositories can run;
- poison-operator-checkout and failpoint tests before/inside/after every Git
  child and phase R/A/B/P/F/C; every resume converges without modifying operator
  tracked/staged/untracked state or duplicating a phase commit;
- final-mode tests for `Squash`, `MergeCommit`, private-workspace `Stage`,
  explicit-finalize `Manual`, stale-base reconciliation, reset/archive, and
  conflicts, including CAS, reachability/provenance, and integration-status assertions;
- scaffold and `.claude` workflow conformance tests whose generated bundles pass the Rust loader;
- a clean-removal search plus `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, and the repository's JavaScript workflow tests.

## Interaction with prior and later work

- **Preserves existing scheduler and review concepts.** The plan changes their source boundary and durable bookkeeping, not the basic develop→review→fix loop.
- **Builds on plan branches and configurable final merge.** Plan 0030's integration branch remains the unit of assembly; this plan makes its commits auditable and defines each final mode's status semantics.
- **Builds on unified navigation and detail panes.** Plans 0031–0032 keep their UX shape but consume typed documents instead of parsing `TASKS.md` snippets.
- **Does not rewrite history.** Plans 0001–0047 stay readable as authored; only plan 0048 and later are executable under the new contract.
- **Prepares the sandbox plan.** Once Makina can express intended write
  footprints, gates, and status reliably, a following plan can introduce
  `makina-sandbox` and a Linux bubblewrap backend. That sandbox must keep the Git
  common directory and coordinator-owned status paths read-only/unavailable to
  worker agents while coordinator Git runs outside it; `touches` alone is not
  isolation.
