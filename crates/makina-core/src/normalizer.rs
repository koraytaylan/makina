//! Model-driven TASKS.md normalizer for repairing malformed or missing task lists.
//!
//! When a plan directory follows the plan convention (SCOPE.md + ARCHITECTURE.md),
//! but TASKS.md is missing or unparseable, the [`ModelNormalizer`] invokes a
//! language-model agent to repair or generate it from the plan brief.
//!
//! # Design
//!
//! The normalizer is a one-shot agent session (parallel to the planner's
//! interpretation path). It reads the plan's SCOPE.md and ARCHITECTURE.md,
//! builds a repair prompt, and sends it to the backend. The model's response
//! is expected to be canonical Makina-convention TASKS.md text (not JSON).

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use thiserror::Error;

use crate::backend::{AgentBackend, Prompt, ResponseEvent, SessionConfig};
use crate::constants::PLANNER_NORMALIZE_SYSTEM_PROMPT;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that [`ModelNormalizer::normalize`] may return.
#[derive(Debug, Error)]
pub enum NormalizeError {
    /// Failed to read plan spec files (SCOPE.md or ARCHITECTURE.md).
    #[error("failed to read plan spec: {0}")]
    ReadError(#[from] std::io::Error),

    /// The planner backend failed at the transport/session level.
    #[error("planner backend error: {0}")]
    BackendError(String),

    /// The planner response was invalid or empty.
    #[error("planner response invalid: {0}")]
    InvalidResponse(String),
}

// ── Normalizer struct ──────────────────────────────────────────────────────────

/// Model-driven TASKS.md normalizer.
///
/// Repairs or generates a canonical TASKS.md for a plan directory from its
/// SCOPE.md + ARCHITECTURE.md brief.
pub struct ModelNormalizer {
    /// The agent backend used to spawn sessions.
    backend: Arc<dyn AgentBackend>,
}

impl ModelNormalizer {
    /// Create a new `ModelNormalizer` with the given backend.
    pub fn new(backend: Arc<dyn AgentBackend>) -> Self {
        ModelNormalizer { backend }
    }

    /// Normalize a malformed or missing TASKS.md by invoking the planner
    /// to repair/generate it from the SCOPE.md + ARCHITECTURE.md brief.
    ///
    /// # Parameters
    ///
    /// - `plan_dir` — The plan directory containing SCOPE.md and ARCHITECTURE.md.
    /// - `slug` — The file-stem identifier for the plan (e.g., `"0031-Sidebar"`).
    ///   Included in the prompt context so the planner can reference the plan by
    ///   name, and recorded as the `task_id` in the session config for log
    ///   correlation.
    ///
    /// # Returns
    ///
    /// On success, returns the normalized TASKS.md content as a string, ready to
    /// be written to disk. On failure, returns a [`NormalizeError`].
    pub async fn normalize(&self, plan_dir: &Path, slug: &str) -> Result<String, NormalizeError> {
        // 1. Read SCOPE.md + ARCHITECTURE.md
        let scope = tokio::fs::read_to_string(plan_dir.join("SCOPE.md")).await?;
        let arch = tokio::fs::read_to_string(plan_dir.join("ARCHITECTURE.md")).await?;
        let brief = format!("{scope}\n\n{arch}");

        // 2. Try to read existing TASKS.md to include error context
        let existing = match tokio::fs::read_to_string(plan_dir.join("TASKS.md")).await {
            Ok(content) => Some(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(NormalizeError::ReadError(e)),
        };

        // 3. Build the prompt for the planner
        let prompt_text = format!(
            "Plan: {slug}\n\n{}\n\n{}\n\nGenerate a canonical TASKS.md conforming to the Makina convention.",
            brief,
            if let Some(ref text) = existing {
                format!("Existing TASKS.md (may be malformed):\n{}", text)
            } else {
                "No existing TASKS.md found.".to_string()
            }
        );

        // 4. Spawn a session and call planner
        let config = SessionConfig {
            working_dir: std::path::PathBuf::from("/tmp"),
            system_prompt: PLANNER_NORMALIZE_SYSTEM_PROMPT.to_string(),
            mode: None,
            model: None,
            effort: None,
            extra: None,
            task_id: Some(slug.to_string()),
        };
        let mut session = self
            .backend
            .spawn(config)
            .await
            .map_err(|e| NormalizeError::BackendError(e.to_string()))?;

        // 5. Send the prompt and collect the response
        let mut stream = session
            .prompt(Prompt::new(prompt_text))
            .await
            .map_err(|e| NormalizeError::BackendError(e.to_string()))?;

        let mut response = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ResponseEvent::TextChunk { text }) => response.push_str(&text),
                Ok(ResponseEvent::ThoughtChunk { .. })
                | Ok(ResponseEvent::ToolCall { .. })
                | Ok(ResponseEvent::ToolCallUpdate { .. })
                | Ok(ResponseEvent::CurrentModeUpdate { .. }) => {}
                Ok(ResponseEvent::TurnComplete { .. }) => break,
                Err(e) => {
                    return Err(NormalizeError::BackendError(e.to_string()));
                }
            }
        }
        drop(stream);

        // 6. Terminate the session (best-effort)
        let _ = session.terminate().await;

        // 7. Return the response (should be canonical TASKS.md)
        if response.trim().is_empty() {
            Err(NormalizeError::InvalidResponse(
                "planner returned empty response".to_string(),
            ))
        } else {
            Ok(response)
        }
    }
}
