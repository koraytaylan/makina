//! Cross-cutting dependency inference for task graphs.
//!
//! # Overview
//!
//! [`EdgeInferrer`] is a decorator over any [`TaskListInterpreter`] that adds
//! inferred dependency edges to the resulting [`TaskGraph`].  It calls the inner
//! interpreter, then post-processes the graph with [`infer_edges`] to serialize
//! tasks that touch the same files or areas.
//!
//! This is Makina's primary defence against same-file merge conflicts: overlapping
//! tasks are serialized at planning time so their branches never collide.
//!
//! # Area-extraction heuristic
//!
//! The area of a task is the set of **backtick-delimited code spans** found in
//! its text (`title` + `description` + `done_when`).  The structured-text
//! convention already wraps crate names, file names, and identifiers in
//! backticks (e.g. `` `makina-core` ``, `` `backend.rs` ``, `` `AgentBackend` ``),
//! so these spans are a reliable, zero-cost proxy for "what does this task touch".
//!
//! Spans are normalized (trimmed + lowercased) before comparison.  A task with no
//! backtick spans has an empty area set and will never be linked by inference
//! (explicit `Depends on` edges are unaffected).
//!
//! **Conservative bias** — over-serializing (an extra edge) is safe because it
//! only delays a task; under-serializing risks a merge conflict when two tasks
//! modify the same file simultaneously.  The heuristic therefore errs toward
//! adding an edge whenever areas overlap.
//!
//! Note: high-frequency tokens like `` `makina-core` `` are weak discriminators
//! and may over-serialize a plan by linking many unrelated tasks. This
//! is acceptable for the deterministic baseline but worth flagging; a future
//! model-backed area extractor can narrow the signal.
//!
//! # Edge-inference rule
//!
//! For every pair of tasks (A, B) that share at least one area:
//! - If A appears before B in `tasks` order, add B → A (B depends on A),
//!   **unless** adding that edge would create a cycle.
//! - Edges are only added, never removed.  All explicit `Depends on` edges are
//!   preserved.
//! - Duplicate edges are suppressed (no edge is added if one already exists).
//! - Self-edges are never added.
//!
//! # Acyclicity guarantee
//!
//! The resulting graph is acyclic because [`infer_edges`] applies a **reachability
//! guard** before adding any inferred edge: it only adds "later depends on earlier"
//! if `earlier` does not already transitively depend on `later` (which would close
//! a cycle).  Explicit forward `Depends on` edges are handled correctly — if a
//! task authored earlier explicitly depends on a later-authored task, the two are
//! already serialized in the opposite order, so the would-be inferred backward edge
//! is safely skipped.
//!
//! # Future work
//!
//! The backtick heuristic is the **deterministic baseline** (no model, no NLP).
//! Task 18 (`planner-model-mechanism`) may compose a model-backed interpreter
//! *before* this decorator, improving area extraction precision while keeping
//! the serialization rule here unchanged.  Alternatively, a future task may
//! replace the heuristic inside `infer_edges` with a model call that returns
//! per-task file lists — the [`EdgeInferrer`] wrapper and the
//! [`TaskListInterpreter`] seam remain stable either way.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::interpreter::{InterpretError, TaskListInterpreter};
use crate::task::{TaskGraph, TaskId};

/// An inferred serialization edge, kept separate from authored dependencies
/// for diagnostics and presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollisionEdge {
    pub prerequisite: TaskId,
    pub dependent: TaskId,
    pub left_pattern: String,
    pub right_pattern: String,
}

/// Match a repository-relative path using Plan 0048's portable grammar.
/// Patterns have already crossed the validated [`crate::plan::RepoPattern`]
/// boundary: literals, `*` inside one segment, and terminal `/**` only.
pub fn footprint_matches(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<_> = pattern.split('/').collect();
    let path_parts: Vec<_> = path.split('/').collect();
    let recursive = pattern_parts.last() == Some(&"**");
    let fixed = if recursive {
        &pattern_parts[..pattern_parts.len() - 1]
    } else {
        &pattern_parts[..]
    };
    if (!recursive && fixed.len() != path_parts.len())
        || (recursive && path_parts.len() < fixed.len())
    {
        return false;
    }
    fixed
        .iter()
        .zip(path_parts.iter())
        .all(|(pattern, value)| segment_matches(pattern, value))
}

fn segment_matches(pattern: &str, value: &str) -> bool {
    let (mut p, mut v) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    let p_bytes = pattern.as_bytes();
    let v_bytes = value.as_bytes();
    while v < v_bytes.len() {
        if p < p_bytes.len() && p_bytes[p] == v_bytes[v] {
            p += 1;
            v += 1;
        } else if p < p_bytes.len() && p_bytes[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(star_at) = star {
            retry += 1;
            v = retry;
            p = star_at + 1;
        } else {
            return false;
        }
    }
    while p < p_bytes.len() && p_bytes[p] == b'*' {
        p += 1;
    }
    p == p_bytes.len()
}

/// Conservative, exact overlap test for the supported grammar. Literal
/// parent/child paths intentionally collide because replacing a directory or
/// an entry below it cannot safely run concurrently.
pub fn footprints_overlap(left: &str, right: &str) -> bool {
    if left == right || is_component_prefix(left, right) || is_component_prefix(right, left) {
        return true;
    }
    let l: Vec<_> = left.split('/').collect();
    let r: Vec<_> = right.split('/').collect();
    let l_recursive = l.last() == Some(&"**");
    let r_recursive = r.last() == Some(&"**");
    let l_fixed = if l_recursive {
        &l[..l.len() - 1]
    } else {
        &l[..]
    };
    let r_fixed = if r_recursive {
        &r[..r.len() - 1]
    } else {
        &r[..]
    };
    let shared = l_fixed.len().min(r_fixed.len());
    if !(0..shared).all(|i| segments_intersect(l_fixed[i], r_fixed[i])) {
        return false;
    }
    l_fixed.len() == r_fixed.len()
        || (l_fixed.len() < r_fixed.len() && l_recursive)
        || (r_fixed.len() < l_fixed.len() && r_recursive)
}

fn is_component_prefix(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn segments_intersect(left: &str, right: &str) -> bool {
    // Supported segment globs contain only literals and `*`. Product-state
    // reachability decides whether some finite byte string matches both.
    let a = left.as_bytes();
    let b = right.as_bytes();
    let mut stack = vec![(0usize, 0usize)];
    let mut seen = HashSet::new();
    while let Some((i, j)) = stack.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        if i == a.len() && j == b.len() {
            return true;
        }
        if i < a.len() && a[i] == b'*' {
            stack.push((i + 1, j));
        }
        if j < b.len() && b[j] == b'*' {
            stack.push((i, j + 1));
        }
        if i < a.len() && j < b.len() {
            match (a[i], b[j]) {
                (b'*', b'*') => {}
                (b'*', _) => stack.push((i, j + 1)),
                (_, b'*') => stack.push((i + 1, j)),
                (x, y) if x == y => stack.push((i + 1, j + 1)),
                _ => {}
            }
        }
    }
    false
}

/// Add collision edges from explicit footprints. A single deterministic
/// topological extension of the authored DAG fixes every direction, preventing
/// pair-local choices from closing a cycle.
pub fn infer_footprint_edges(
    graph: &mut TaskGraph,
    footprints: &HashMap<TaskId, Vec<String>>,
) -> Result<Vec<CollisionEdge>, String> {
    let order = authored_topological_order(graph)?;
    let authored = graph.clone();
    let mut inferred = Vec::new();
    for left in 0..order.len() {
        for right in (left + 1)..order.len() {
            let prerequisite = order[left].clone();
            let dependent = order[right].clone();
            if transitive_depends_on(&authored, &prerequisite, &dependent)
                || transitive_depends_on(&authored, &dependent, &prerequisite)
            {
                continue;
            }
            let Some((lp, rp)) = footprints
                .get(&prerequisite)
                .into_iter()
                .flatten()
                .find_map(|lp| {
                    footprints
                        .get(&dependent)
                        .into_iter()
                        .flatten()
                        .find_map(|rp| footprints_overlap(lp, rp).then(|| (lp.clone(), rp.clone())))
                })
            else {
                continue;
            };
            graph
                .tasks
                .iter_mut()
                .find(|task| task.id == dependent)
                .expect("task came from graph")
                .depends_on
                .push(prerequisite.clone());
            inferred.push(CollisionEdge {
                prerequisite,
                dependent,
                left_pattern: lp,
                right_pattern: rp,
            });
        }
    }
    if authored_topological_order(graph).is_err() {
        return Err("inferred collision edges made the graph cyclic".into());
    }
    Ok(inferred)
}

/// Augment directly from metadata retained on the runtime graph.
pub fn infer_authored_footprint_edges(graph: &mut TaskGraph) -> Result<Vec<CollisionEdge>, String> {
    let authored_graph = graph.clone();
    let footprints = graph
        .authored
        .iter()
        .map(|(id, metadata)| {
            (
                id.clone(),
                metadata
                    .touches
                    .iter()
                    .map(|pattern| pattern.as_str().to_owned())
                    .collect(),
            )
        })
        .collect();
    let mut edges = infer_footprint_edges(graph, &footprints)?;
    let order = authored_topological_order(&authored_graph)?;
    let areas: HashMap<_, _> = authored_graph
        .tasks
        .iter()
        .map(|task| (task.id.clone(), extract_areas(&task_text(task))))
        .collect();
    for left in 0..order.len() {
        for right in (left + 1)..order.len() {
            let prerequisite = &order[left];
            let dependent = &order[right];
            let suspicious = |id: &TaskId| {
                graph.authored.get(id).is_none_or(|metadata| {
                    metadata.touches.is_empty()
                        || metadata.touches.iter().any(|pattern| {
                            matches!(pattern, crate::task::AuthoredRepoPattern::InertCandidate(_))
                        })
                })
            };
            if !(suspicious(prerequisite) || suspicious(dependent))
                || areas[prerequisite].is_disjoint(&areas[dependent])
                || transitive_depends_on(graph, prerequisite, dependent)
                || transitive_depends_on(graph, dependent, prerequisite)
            {
                continue;
            }
            graph
                .tasks
                .iter_mut()
                .find(|task| &task.id == dependent)
                .expect("topological task came from graph")
                .depends_on
                .push(prerequisite.clone());
            tracing::warn!(
                prerequisite = %prerequisite,
                dependent = %dependent,
                "suspicious authored footprint serialized by conservative textual fallback"
            );
            edges.push(CollisionEdge {
                prerequisite: prerequisite.clone(),
                dependent: dependent.clone(),
                left_pattern: "<textual-fallback>".into(),
                right_pattern: "<textual-fallback>".into(),
            });
        }
    }
    for edge in &edges {
        if let Some(metadata) = graph.authored.get_mut(&edge.dependent) {
            metadata
                .collision_dependencies
                .push(edge.prerequisite.clone());
        }
    }
    Ok(edges)
}

fn authored_topological_order(graph: &TaskGraph) -> Result<Vec<TaskId>, String> {
    let mut remaining: HashMap<TaskId, usize> = graph
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.depends_on.len()))
        .collect();
    let mut result = Vec::with_capacity(graph.tasks.len());
    while result.len() < graph.tasks.len() {
        let next = graph
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| remaining.get(&task.id) == Some(&0))
            .min_by_key(|(source, task)| (numeric_key(&task.id.0), *source))
            .map(|(_, task)| task.id.clone())
            .ok_or_else(|| "authored dependency graph contains a cycle".to_string())?;
        remaining.remove(&next);
        result.push(next.clone());
        for task in &graph.tasks {
            if task.depends_on.contains(&next) {
                if let Some(value) = remaining.get_mut(&task.id) {
                    *value -= 1;
                }
            }
        }
    }
    Ok(result)
}

fn numeric_key(id: &str) -> u64 {
    id.split_once('-')
        .and_then(|(prefix, _)| prefix.parse().ok())
        .unwrap_or(u64::MAX)
}

// ── EdgeInferrer ──────────────────────────────────────────────────────────────

/// A decorator over [`TaskListInterpreter`] that adds cross-cutting dependency
/// edges to the returned [`TaskGraph`].
///
/// Construct with [`EdgeInferrer::new`], passing any existing interpreter.
/// The resulting `EdgeInferrer` satisfies `TaskListInterpreter` itself, so it
/// can be injected wherever an interpreter is expected.
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use makina_core::interpreter::SourceProjectionUnavailable;
/// use makina_core::dependency::EdgeInferrer;
///
/// let interpreter = EdgeInferrer::new(Arc::new(SourceProjectionUnavailable::new()));
/// // `interpreter` now wraps the deterministic parser with inferred edges.
/// ```
///
pub struct EdgeInferrer {
    inner: Arc<dyn TaskListInterpreter>,
}

impl EdgeInferrer {
    /// Wrap an existing interpreter with cross-cutting dependency inference.
    pub fn new(inner: Arc<dyn TaskListInterpreter>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl TaskListInterpreter for EdgeInferrer {
    /// Interpret the task list via the inner interpreter, then add inferred
    /// edges for tasks that share areas (backtick spans), then re-validate.
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError> {
        let mut graph = self.inner.interpret(slug, source_text).await?;
        infer_edges(&mut graph);
        // Validate that the augmented graph is still structurally sound.
        // (The acyclicity guarantee means this should always pass, but we run it
        // defensively to catch any bug in the inference logic early.)
        graph.validate()?;
        Ok(graph)
    }
}

// ── Area extraction ───────────────────────────────────────────────────────────

/// Extract the set of areas from a block of text.
///
/// An "area" is any backtick-delimited span found in `text`, normalized to a
/// trimmed, lowercase string.  Spans that are empty after trimming are dropped.
///
/// # Example
///
/// ```ignore
/// let areas = extract_areas("Add `AgentBackend` to `makina-core`.");
/// // → {"agentbackend", "makina-core"}
/// ```
fn extract_areas(text: &str) -> HashSet<String> {
    let mut areas = HashSet::new();
    let mut chars = text.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        if ch == '`' {
            // Consume until the closing backtick or end-of-string.
            let start = i + 1; // byte offset after the opening backtick
            let mut end = start;
            for (j, c) in chars.by_ref() {
                if c == '`' {
                    end = j;
                    break;
                }
                end = j + c.len_utf8();
            }
            let span = text[start..end].trim().to_lowercase();
            if !span.is_empty() {
                areas.insert(span);
            }
        }
    }

    areas
}

/// Combine all text fields of a task into one string for area extraction.
fn task_text(task: &crate::task::Task) -> String {
    format!("{} {} {}", task.title, task.description, task.done_when)
}

// ── Edge inference ────────────────────────────────────────────────────────────

/// Add inferred dependency edges to `graph` for tasks that share at least one
/// area (backtick span).
///
/// See the [module documentation](self) for the full rule, conservative-bias
/// rationale, and the acyclicity guarantee.
pub fn infer_edges(graph: &mut TaskGraph) {
    let n = graph.tasks.len();
    if n < 2 {
        return;
    }

    // Compute areas for every task up front (index → set of area strings).
    let areas: Vec<HashSet<String>> = graph
        .tasks
        .iter()
        .map(|t| extract_areas(&task_text(t)))
        .collect();

    // For each pair (i < j), if they share an area, add j → i (j depends on i),
    // subject to the reachability guard below.
    //
    // We apply additions one at a time (not batched) so that reachability checks
    // account for edges inferred earlier in this same pass.
    for i in 0..n {
        for j in (i + 1)..n {
            // Skip if they share no areas.
            if areas[i].is_disjoint(&areas[j]) {
                continue;
            }

            let earlier_id = graph.tasks[i].id.clone();
            let later_id = graph.tasks[j].id.clone();

            // Skip if the edge j → i already exists (explicit or previously inferred).
            let already_exists = graph.tasks[j]
                .depends_on
                .iter()
                .any(|dep| dep == &earlier_id);

            if already_exists {
                continue;
            }

            // Reachability guard: adding "later depends on earlier" is only safe
            // (acyclic) if `earlier` does NOT already transitively depend on `later`.
            // If earlier ⇒ ... ⇒ later already exists, the two tasks are serialized
            // in the opposite order by explicit edges, so the inferred backward edge
            // is both unnecessary and would close a cycle — skip it.
            if transitive_depends_on(graph, &earlier_id, &later_id) {
                continue;
            }

            graph.tasks[j].depends_on.push(earlier_id);
        }
    }
}

/// Returns `true` if `start` transitively depends on `target` by following
/// `depends_on` edges in the current state of `graph` (DFS).
///
/// Used by [`infer_edges`] as a reachability guard to prevent cycles.
/// Also used by the structural validator for cycle detection via self-reachability.
pub(crate) fn transitive_depends_on(graph: &TaskGraph, start: &TaskId, target: &TaskId) -> bool {
    // Build a quick id→task index map for efficient lookup.
    let index_of: HashMap<&TaskId, usize> = graph
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (&t.id, i))
        .collect();

    // Iterative DFS using an explicit stack.
    let mut visited: HashSet<&TaskId> = HashSet::new();
    let mut stack: Vec<&TaskId> = Vec::new();

    // Seed the stack with start's direct dependencies.
    if let Some(&idx) = index_of.get(start) {
        for dep in &graph.tasks[idx].depends_on {
            stack.push(dep);
        }
    }

    while let Some(current) = stack.pop() {
        if current == target {
            return true;
        }
        if visited.insert(current)
            && let Some(&idx) = index_of.get(current)
        {
            for dep in &graph.tasks[idx].depends_on {
                if !visited.contains(dep) {
                    stack.push(dep);
                }
            }
        }
    }

    false
}

// ── Tests ─────────────────────────────────────────────────────────────────────
