//! Gate execution for the Developer-side gate-iteration loop (task 22).
//!
//! # What a gate is
//!
//! A **gate** is an exit-code-zero shell command (e.g. `cargo test`,
//! `cargo clippy -- -D warnings`, `cargo fmt --check`) configured in
//! `makina.toml` as a [`GateConfig`](crate::config::GateConfig).  A gate
//! "passes" iff its command exits `0`.  Gates are deliberately nothing more
//! than this — there is no rule engine, no structured assertions, no parsing of
//! the command's output for meaning.  Richer rule kinds are explicitly a FUTURE
//! concern.
//!
//! # What this module does
//!
//! [`GateRunner`] runs the configured gates, in order, in a given working
//! directory (the task's git worktree).  Each gate command is executed via
//! `sh -c "<command>"` so the operator can write an ordinary shell command line
//! (pipes, `&&`, env expansion, etc.) exactly as they would on the terminal.
//!
//! - All gates exit `0` → [`GateOutcome::Passed`].
//! - The **first** gate that exits non-zero → [`GateOutcome::Failed`] carrying
//!   that gate's name, its combined stdout+stderr (truncated if very large), and
//!   its exit code.  Evaluation **stops at the first failure**: that failure is
//!   the one fed back to the agent.
//! - An empty gate list → [`GateOutcome::Passed`] (nothing to check).
//!
//! Note on the architecture's "re-run ALL gates after each fix": stopping at the
//! first failure here is correct because the Supervisor's gate loop re-invokes
//! [`GateRunner::run_gates`] **from the top** on the next iteration (after the
//! agent fixes the reported failure), so every gate is re-evaluated each round.
//! This module's single responsibility is one *pass* over the gate list.
//!
//! # Errors
//!
//! A gate *failing* (non-zero exit) is a normal outcome, NOT an error — it is
//! reported via [`GateOutcome::Failed`].  [`GateRunnerError`] is reserved for the
//! case where the command could not even be **launched** (e.g. `sh` is missing,
//! or the working directory does not exist), which is an infrastructure problem
//! rather than a gate result.
//!
//! # Placement note (architecture)
//!
//! The architecture frames gates as "Developer-side" (the Developer iterates
//! until gates pass before handing the work back).  For the MVP the gate *loop*
//! is implemented **Supervisor-coordinated**: the Supervisor runs these gates and
//! re-dispatches the Developer with the failure output to fix.  The agent still
//! does the fixing; the gate **execution** is this reusable [`GateRunner`].  See
//! [`crate::actors::supervisor`] for the loop itself.

use std::path::Path;

use thiserror::Error;

use crate::config::GateConfig;

/// Maximum number of bytes of combined gate output retained in
/// [`GateOutcome::Failed`].
///
/// Gate commands (test suites, linters) can emit a great deal of output.  We
/// keep only the **last** `OUTPUT_TRUNCATION_LIMIT` bytes because the tail
/// (final errors, failure summary) is the most actionable part to feed back to
/// the agent, and an unbounded blob would bloat the prompt.  When truncation
/// occurs a short marker is prepended so it is obvious output was dropped.
const OUTPUT_TRUNCATION_LIMIT: usize = 16 * 1024;

// ── GateRunnerError ─────────────────────────────────────────────────────────────

/// An error that prevented a gate command from being **launched** at all.
///
/// This is distinct from a gate *failing* (a non-zero exit), which is a normal
/// [`GateOutcome::Failed`] result rather than an error.  `GateRunnerError` means
/// the OS could not start the subprocess (e.g. `sh` not found, working directory
/// missing, permission denied) — an infrastructure failure the caller surfaces
/// as a hard error for the task.
#[derive(Debug, Error)]
pub enum GateRunnerError {
    /// The gate's shell command could not be spawned.
    ///
    /// `gate` is the failing gate's configured name and `command` is the shell
    /// command line that could not be launched; `message` is the underlying
    /// `std::io::Error`.
    #[error("failed to launch gate `{gate}` (command: {command:?}): {message}")]
    Launch {
        /// The configured name of the gate whose command could not start.
        gate: String,
        /// The shell command line that failed to launch.
        command: String,
        /// The underlying OS error message.
        message: String,
    },
}

// ── GateOutcome ───────────────────────────────────────────────────────────────

/// The result of one pass over the configured gates.
///
/// Either every gate passed, or the first failing gate is reported with enough
/// detail to feed back to the agent for a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Every configured gate exited `0` (or there were no gates).  The work is
    /// ready to advance to review.
    Passed,

    /// The first gate that exited non-zero, with the detail needed to fix it.
    ///
    /// Evaluation stopped at this gate; later gates were **not** run this pass
    /// (they will be re-evaluated on the next pass, once the agent has fixed
    /// this failure).
    Failed {
        /// The configured name of the gate that failed (e.g. `"tests"`).
        gate: String,
        /// Combined stdout + stderr from the failing command, truncated to the
        /// last [`OUTPUT_TRUNCATION_LIMIT`] bytes if very large.
        output: String,
        /// The process exit code, or `-1` if the process was terminated by a
        /// signal with no exit code (rare; preserved so the caller can report
        /// *something* numeric).
        exit_code: i32,
    },
}

// ── GateRunner ──────────────────────────────────────────────────────────────────

/// Runs the configured gates in a working directory.
///
/// Stateless: a single [`GateRunner`] can be reused across tasks and iterations.
/// It is constructed with [`GateRunner::new`] (or [`Default`]).
///
/// # Example
///
/// ```rust,no_run
/// # use std::path::Path;
/// # use makina_core::gate::{GateRunner, GateOutcome};
/// # use makina_core::config::GateConfig;
/// # async fn example() -> Result<(), makina_core::gate::GateRunnerError> {
/// let runner = GateRunner::new();
/// let gates = vec![GateConfig {
///     name: "tests".into(),
///     command: "cargo test --workspace".into(),
///     image: None,
/// }];
/// match runner.run_gates(&gates, Path::new("/path/to/worktree")).await? {
///     GateOutcome::Passed => { /* advance to review */ }
///     GateOutcome::Failed { gate, output, exit_code } => {
///         eprintln!("gate {gate} failed ({exit_code}):\n{output}");
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct GateRunner;

impl GateRunner {
    /// Create a new gate runner.
    pub fn new() -> Self {
        Self
    }

    /// Run `gates` in order inside `working_dir`, stopping at the first failure.
    ///
    /// Each gate's `command` is executed as `sh -c "<command>"` via
    /// [`tokio::process::Command`] with `current_dir(working_dir)`, capturing
    /// stdout and stderr.
    ///
    /// # Returns
    ///
    /// - [`GateOutcome::Passed`] if every gate exits `0` (or `gates` is empty).
    /// - [`GateOutcome::Failed`] for the **first** gate that exits non-zero,
    ///   carrying its name, combined output, and exit code.  Subsequent gates
    ///   are not run this pass.
    ///
    /// # Errors
    ///
    /// [`GateRunnerError::Launch`] if a gate command could not be spawned at all
    /// (e.g. `sh` missing or `working_dir` does not exist).  A gate that *runs*
    /// and exits non-zero is reported via [`GateOutcome::Failed`], not as an
    /// error.
    pub async fn run_gates(
        &self,
        gates: &[GateConfig],
        working_dir: &Path,
    ) -> Result<GateOutcome, GateRunnerError> {
        for gate in gates {
            // Execute the gate's command line through a shell so operators can
            // use ordinary shell syntax (pipes, &&, env expansion, etc.).
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&gate.command)
                .current_dir(working_dir)
                .output()
                .await
                .map_err(|e| GateRunnerError::Launch {
                    gate: gate.name.clone(),
                    command: gate.command.clone(),
                    message: e.to_string(),
                })?;

            if !output.status.success() {
                // First failing gate: this is the one fed back to the agent.
                let combined = combine_output(&output.stdout, &output.stderr);
                return Ok(GateOutcome::Failed {
                    gate: gate.name.clone(),
                    output: truncate_tail(&combined, OUTPUT_TRUNCATION_LIMIT),
                    // `code()` is None when the process was killed by a signal;
                    // fall back to -1 so the caller always has a number.
                    exit_code: output.status.code().unwrap_or(-1),
                });
            }
        }

        // All gates passed (or there were none).
        Ok(GateOutcome::Passed)
    }
}

// ── Output helpers ──────────────────────────────────────────────────────────────

/// Combine a command's stdout and stderr into a single string for feedback.
///
/// Both streams are decoded lossily (gate output is human text, not guaranteed
/// UTF-8).  stdout is shown first, then stderr under a label, so the agent sees
/// the full picture.  Empty streams are omitted to keep the blob compact.
fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);

    let out = out.trim_end();
    let err = err.trim_end();

    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.to_string(),
        (true, false) => format!("stderr:\n{err}"),
        (false, false) => format!("{out}\nstderr:\n{err}"),
    }
}

/// Keep only the last `limit` bytes of `s`, prepending a marker if truncated.
///
/// The tail is retained because failure summaries (the actionable part) appear
/// at the end of most tool output.  Truncation respects UTF-8 char boundaries so
/// the result is always valid UTF-8.
fn truncate_tail(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }

    // Find a char boundary at or after (len - limit) so we don't split a
    // multi-byte character.
    let mut start = s.len() - limit;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }

    format!("[output truncated to last {limit} bytes]\n{}", &s[start..])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for [`GateRunner`] output/semantics helpers and the
    //! pass/first-failure behaviour.
    //!
    //! These run trivial shell builtins (`true`, `false`, `echo`) in a
    //! `tempfile` directory — deterministic, fast, and side-effect-free.  The
    //! full Supervisor-integrated gate *loop* is exercised by the
    //! `tests/gate_runner.rs` integration tests.

    use super::*;

    fn gate(name: &str, command: &str) -> GateConfig {
        GateConfig {
            name: name.to_string(),
            command: command.to_string(),
            image: None,
        }
    }

    #[tokio::test]
    async fn empty_gate_list_passes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let outcome = GateRunner::new()
            .run_gates(&[], dir.path())
            .await
            .expect("launch ok");
        assert_eq!(outcome, GateOutcome::Passed);
    }

    #[tokio::test]
    async fn all_passing_gates_pass() {
        let dir = tempfile::tempdir().expect("temp dir");
        let gates = vec![gate("a", "true"), gate("b", "exit 0")];
        let outcome = GateRunner::new()
            .run_gates(&gates, dir.path())
            .await
            .expect("launch ok");
        assert_eq!(outcome, GateOutcome::Passed);
    }

    #[tokio::test]
    async fn first_failing_gate_is_reported_and_stops_evaluation() {
        let dir = tempfile::tempdir().expect("temp dir");
        // 2nd gate fails; 3rd would crash the test if it ran (it writes a file
        // we then assert does NOT exist), proving evaluation stops at the first
        // failure.
        let sentinel = dir.path().join("third-ran");
        let third_cmd = format!("touch {}", sentinel.display());
        let gates = vec![
            gate("first", "true"),
            gate("second", "echo boom-out; echo boom-err 1>&2; exit 3"),
            gate("third", &third_cmd),
        ];

        let outcome = GateRunner::new()
            .run_gates(&gates, dir.path())
            .await
            .expect("launch ok");

        match outcome {
            GateOutcome::Failed {
                gate,
                output,
                exit_code,
            } => {
                assert_eq!(gate, "second", "the first failing gate must be reported");
                assert_eq!(exit_code, 3, "exit code must be captured");
                assert!(
                    output.contains("boom-out"),
                    "combined output must include stdout; got: {output:?}"
                );
                assert!(
                    output.contains("boom-err"),
                    "combined output must include stderr; got: {output:?}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        assert!(
            !sentinel.exists(),
            "the gate after the first failure must NOT run"
        );
    }

    #[tokio::test]
    async fn launch_error_when_working_dir_missing() {
        let missing = Path::new("/this/path/definitely/does/not/exist/makina");
        let gates = vec![gate("a", "true")];
        let err = GateRunner::new()
            .run_gates(&gates, missing)
            .await
            .expect_err("spawning in a missing dir must error");
        assert!(matches!(err, GateRunnerError::Launch { .. }));
    }

    #[test]
    fn combine_output_labels_stderr() {
        assert_eq!(combine_output(b"out", b""), "out");
        assert_eq!(combine_output(b"", b"err"), "stderr:\nerr");
        assert_eq!(combine_output(b"out", b"err"), "out\nstderr:\nerr");
        assert_eq!(combine_output(b"", b""), "");
    }

    #[test]
    fn truncate_tail_keeps_the_end() {
        let s = "a".repeat(100);
        let out = truncate_tail(&s, 10);
        assert!(out.contains("truncated"), "marker must be present");
        assert!(
            out.ends_with(&"a".repeat(10)),
            "must keep the last 10 bytes"
        );

        // Short strings pass through untouched.
        assert_eq!(truncate_tail("short", 10), "short");
    }
}
