# Makina Trial — End-to-End Run (task 33, `e2e-run`)

This is the culmination of the build: the trial task list driven through the
**full loop** — Planner → Supervisor → Developer + gates → Reviewer →
squash-merge — by the same `Arc<dyn Api>` (`CoreApi`) the TUI drives, against a
**real authenticated ACP agent** (`gemini --acp`).

There are two equivalent ways to run it:

1. **Automated harness** (`crates/makina/tests/e2e.rs`) — drives `CoreApi`
   directly (the exact backend the TUI binds to) and asserts the done-when. This
   is the faithful, repeatable proof.
2. **Interactive TUI** — `cargo run -p makina`, open the list, press `s`, watch.
   This is the human-facing equivalent.

> **Done when:** at least one task reaches `done` and lands on `develop`, driven
> from the TUI.

---

## 0. Safety — never run the loop on the live repo

The loop creates `task/{id}` branches + `.makina/worktrees/{id}/` checkouts and
**squash-merges into `develop`**. That mutates the repository. Run it against a
**throwaway clone**, never your working copy:

```bash
git clone /path/to/makina /tmp/makina-trial
cd /tmp/makina-trial          # develop is checked out; the dogfood list is present
```

The automated harness does this clone-into-a-tempdir for you and removes it
afterward; the live repo is only ever read (as the clone source) and is never
used as the engine's `repo_root`.

---

## 1. The trial task list

`docs/trial/dogfood-tasks.md` — two small, safe, self-contained tasks that add
pure helpers to a **new** `crates/makina-core/src/util.rs` (no existing actor,
FSM, or orchestration code is touched):

| id                | adds                                    | depends on        |
|-------------------|-----------------------------------------|-------------------|
| `format-duration` | `pub fn format_duration(secs: u64) -> String` | —           |
| `kebab-validate`  | `pub fn is_valid_kebab_id(s: &str) -> bool`   | `format-duration` |

The deterministic planner (`EdgeInferrer(StructuredTextInterpreter)`) is enough
to plan this well-formed list — no model call is needed for planning. (Both
tasks touch `` `util.rs` ``, so the edge inferrer also serializes them on that
shared area, reinforcing the explicit `format-duration → kebab-validate` edge.)

---

## 2. Gate config — `.makina/config.toml`

The committed project config at the repo root pins the base branch and the three
quality gates (run via `sh -c` in each task's worktree; a task must pass all of
them before review + merge):

```toml
base_branch = "develop"
concurrency = 2

[caps]
gate_iterations     = 5
reviewer_iterations = 3
wall_clock_secs     = 1200

[[gates]]
name    = "test"
command = "cargo test"

[[gates]]
name    = "clippy"
command = "cargo clippy -- -D warnings"

[[gates]]
name    = "fmt"
command = "cargo fmt --check"
```

---

## 3. Global config — `~/.makina/config.toml` (for interactive use)

The **global** layer is machine-specific and NOT committed. It supplies the
agent backend command and the Planner mechanism. The minimum needed to drive
this repo interactively:

```toml
# The ACP agent CLI spawned for Developer + Reviewer sessions. Must be a
# pre-authenticated agent (Zed auth model — Makina holds no credentials).
# `--yolo`: gemini auto-approves its file-edit tool calls. WITHOUT it, gemini
# sends a `session/request_permission` request that Makina's MVP ACP client
# does not answer, and the Developer turn hangs (see §6 "Key finding").
[backend]
command = "gemini"
args    = ["--acp", "--yolo"]

# One-shot-agent is the implemented MVP planner mechanism. (The TUI uses the
# deterministic structured-text interpreter for planning regardless, so this
# only matters if/when a model-backed planner is wired in.)
[planner]
mechanism = "one-shot-agent"

# Optional: defaults if you don't override them in .makina/config.toml.
# concurrency = 2
# [caps]
# gate_iterations     = 5
# reviewer_iterations = 3
# wall_clock_secs     = 1200
```

`Config::load_defaults()` merges `~/.makina/config.toml` (global) with
`./.makina/config.toml` (project, wins on conflict) and validates the result; a missing
or invalid config is fatal at startup.

The agent must already be signed in:

```bash
gemini   # once, interactively, to authenticate; Makina then inherits the env
```

---

## 4. Run it — automated harness (the repeatable proof)

```bash
# --yolo so gemini auto-approves its file-write tool calls (see §6 "Key finding").
MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp,--yolo \
    cargo test -p makina --test e2e -- --ignored --nocapture
```

`MAKINA_ACP_ARGS` is a comma-separated arg list; it defaults to `--acp` if
unset. **Use `--acp,--yolo`** with gemini — with plain `--acp` the Developer
turn hangs on gemini's permission prompt (§6).

What it does (`crates/makina/tests/e2e.rs`):

1. `git clone`s the live repo into a `tempfile::tempdir()`, checks out `develop`,
   and gives the clone a local commit identity.
2. Builds `CoreApi` exactly as `main.rs` does — but with a **real**
   `AcpBackend(gemini, [--acp])` instead of a `NoopBackend`: the deterministic
   `EdgeInferrer(StructuredTextInterpreter)` planner, a `WorktreeManager` rooted
   at the **clone**, and a `Config` with the three gates + `base_branch=develop`
   + generous caps.
3. `execute(OpenRun{ <clone>/docs/trial/dogfood-tasks.md })` then
   `execute(StartRun{run})` — the two commands the TUI issues.
4. Subscribes to the live event stream, logs it (`RunStatusChanged`,
   `TaskStateChanged`, `AgentExchange` prompts/chunks/turns, gate iterations),
   and observes with a generous bounded deadline (20 min).
5. Asserts the done-when: at least one task reaches `Done` **and** a squashed
   `task(...)` commit landed on the clone's `develop`, with the new `util.rs`
   present on `develop` and carrying a dogfood function.

It is `#[ignore]`d so the normal `cargo test` stays fast and green (no agent
needed for CI).

---

## 5. Run it — interactive TUI (the human equivalent)

With `~/.makina/config.toml` and `.makina/config.toml` in place, **in a throwaway clone**:

```bash
cd /tmp/makina-trial
cargo run -p makina
```

Then:

1. Press **`o`** to open the file browser.
2. Navigate to `docs/trial/dogfood-tasks.md` and select it — this issues
   `OpenRun`; the run appears in the sidebar with both tasks `New`/`Ready`.
3. Press **`s`** to start the run (`StartRun`).
4. Watch:
   - the **per-task status** column transition `Ready → InProgress → InReview →
     Done` (and the gate/review iteration counters tick if the agent needs a
     retry);
   - the **live exchange** panel stream the Developer prompt + the agent's
     response chunks, then the Reviewer's verdict;
   - on approval, the task's work squash-merged to `develop` and the task lands
     `Done`.
5. Press **`q`** / `Esc` / `Ctrl-C` to quit.

Because the TUI is pure presentation over `Arc<dyn Api>`, this drives the same
`CoreApi` path the harness drives — the harness is its automated mirror.

Verify the merge afterward (in the clone):

```bash
git -C /tmp/makina-trial log develop --oneline | head
git -C /tmp/makina-trial show develop:crates/makina-core/src/util.rs
```

You should see a `task(format-duration): …` (and/or `task(kebab-validate): …`)
squash commit on `develop` and the new `util.rs` with the helper(s).

---

## 6. Real run result

<!-- RESULT:BEGIN -->
**Outcome: PASS — the done-when was met for real.** A dogfood task ran the full
loop through `CoreApi` + a real `gemini` agent and landed on the clone's
`develop`.

Run: `MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp,--yolo cargo test -p makina
--test e2e -- --ignored --nocapture` (date 2026-05-29). Total time **124.57s**
(~2 min) on a warm `cargo` build cache; `test result: ok. 1 passed`.

What the loop did (observed live via the event stream):

| stage | result |
|-------|--------|
| **interpret** (planner) | ✅ 2 tasks planned: `format-duration` (no deps), `kebab-validate` (deps `format-duration`) — deterministic interpreter, no model call |
| **worktree** | ✅ `task/format-duration` created off `develop` at `.makina/worktrees/format-duration/` |
| **developer** (gemini) | ✅ wrote `crates/makina-core/src/util.rs` with a correct, documented `format_duration` + unit tests, and added `pub mod util;` to `lib.rs` |
| **gates** | ✅ `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all passed first try (gate_iterations=0) |
| **reviewer** (gemini) | ✅ emitted ` ```json {"verdict":"approve"} ``` ` → parsed `Approve` (review_iterations=0) |
| **squash-merge** | ✅ `task(format-duration): Add a human-readable duration formatter` landed on `develop` (42 → **43** commits); worktree torn down |
| **dependency unlock** | ✅ on `format-duration` Done, `kebab-validate` advanced `Ready → InProgress` |

Evidence:
- `git log develop -1 --pretty=%s` → `task(format-duration): Add a human-readable duration formatter`
- `git show develop:crates/makina-core/src/util.rs` → the real `format_duration` (1019 bytes) with `#[cfg(test)] mod tests` covering 0/42/185/3723.
- Event stream: 21 events; states reached `Ready, InProgress, InReview, Done`; 2 agent prompts, 9 response chunks, 2 turns; developer + reviewer both engaged.
- The **live repo was untouched**: it stayed on `develop` @ its pre-run HEAD with no `.makina/worktrees/` (the whole run happened in a `tempfile` clone).

### Key finding — gemini's permission prompt hangs a default-mode turn

The **first** real run (`MAKINA_ACP_ARGS=--acp`, i.e. gemini in its **default**
mode) **hung** on the Developer turn and never wrote a file. Root cause,
confirmed with a standalone ACP probe:

- gemini's `session/new` advertises modes; the default is `currentModeId:
  "default"` whose description is literally *"Prompts for approval"*.
- When the Developer turn asks gemini to write `util.rs`, gemini streams
  *"I will create the `util.rs` file…"* and then sends a **server→client
  request**: `session/request_permission` (with `allow_once` / `allow_always` /
  `reject` options and the pending `write_file` tool call).
- Makina's MVP ACP client advertises **empty `clientCapabilities {}`** and its
  transport **ignores inbound server→client requests**
  (`crates/makina-acp/src/transport.rs`: `IncomingKind::Request => {}`, with the
  comment *"permission prompts are out of scope for the MVP turn; we neither
  answer nor fail on them"*). So gemini blocks forever waiting for a permission
  response that never comes, and the Developer turn stalls until the per-task
  `wall_clock_secs` cap would fire.

Probe proof (newline-delimited JSON-RPC straight to `gemini --acp`):
- default mode, no answer → `session/request_permission` received, file **not**
  written, turn hangs;
- default mode, **answering** the request with `proceed_always` →
  `tool_call_update: completed`, file written, `stopReason: end_turn`;
- **`--yolo`** → `currentModeId: "yolo"`, gemini writes the file via plain
  `session/update` notifications (`tool_call → tool_call_update: completed`),
  **no permission request at all**, turn completes — which is exactly what the
  passing e2e above relied on.

### Workaround used (no engine change)

Pass gemini's `--yolo` (or `--approval-mode yolo`) so it auto-approves tool
calls and never issues `session/request_permission`. This is **pure agent CLI
configuration** via the existing `MAKINA_ACP_ARGS` seam — no change to Makina's
engine or ACP client. For interactive use, set:

```toml
[backend]
command = "gemini"
args    = ["--acp", "--yolo"]
```

> ⚠️ `--yolo` lets the agent run any tool without confirmation. It is fine for a
> sandboxed throwaway clone (as here), but the proper fix is for Makina's ACP
> client to **implement the agent→client permission flow** (answer
> `session/request_permission`, e.g. auto-allow within the isolated worktree, or
> advertise `fs` client capabilities and service `fs/*`). That is a real
> follow-up, captured for task 34.
<!-- RESULT:END -->

---

## 7. Notes for task 34 (trial-findings)

Carry these into the trial-findings writeup:

**What worked**
- The whole orchestration spine is correct end-to-end with a *real* agent:
  deterministic planning, per-task worktree off `develop`, Developer agent edit,
  the three real gates (`cargo test`/`clippy`/`fmt`) passing first try, Reviewer
  verdict parsing, squash-merge to `develop`, worktree teardown, and dependency
  unlock of the next task — all driven through the exact `CoreApi` the TUI uses.
- gemini produced correct, idiomatic, gate-passing Rust on the first attempt for
  `format-duration` (right output formats, doc-comment, unit tests, `pub mod`
  wiring) — 0 gate iterations, 0 review iterations.
- The temp-clone isolation worked perfectly; the live repo was never touched.

**What broke (and the fix)**
- **gemini's default mode hangs the Developer turn.** gemini (default
  `currentModeId: "default"`, *"Prompts for approval"*) sends a
  `session/request_permission` server→client request before writing a file.
  Makina's MVP ACP client advertises empty `clientCapabilities` and **drops
  inbound requests** (`transport.rs: IncomingKind::Request => {}`), so the turn
  blocks forever. Worked around with gemini's `--yolo` (auto-approve, no
  permission request) — pure agent config, no engine change. **Proper fix
  (follow-up):** implement the agent→client permission flow in the ACP client
  (answer `session/request_permission`, e.g. auto-allow inside the isolated
  worktree) and/or advertise `fs` client capabilities and service `fs/*`.

**What was slow / flaky**
- A real agent turn is minutes-scale; a *cold* gate compile (`cargo test` in a
  fresh worktree with no `target/`) adds more. The warm-cache run here was
  ~2 min; budget generously (the harness uses a 20-min observe deadline and a
  1200s per-task `wall_clock_secs` cap).
- gemini prints diagnostic preamble (`[acp-agent] …`) on stdout around the
  protocol stream; the ACP reader already tolerates non-JSON-RPC lines, so this
  is benign but noisy.

The automated harness is the reproducible artifact: re-running it (`--ignored`,
with `--yolo`) re-exercises the whole loop against a fresh clone.
