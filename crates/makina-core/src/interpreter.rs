//! Model-backed graph projection seam retained for runtime role compatibility.
//!
//! Durable plan source is loaded by `plan`; this module has no Markdown parser
//! and no deterministic or offline source fallback.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;

use crate::backend::{AgentBackend, BackendError, Prompt, ResponseEvent, SessionConfig};
use crate::task::{TaskGraph, TaskGraphError};

#[derive(Debug, Error)]
pub enum InterpretError {
    #[error("task graph validation failed: {0}")]
    ValidationFailed(#[from] TaskGraphError),
    #[error("model backend error: {0}")]
    BackendError(#[from] BackendError),
    #[error("model response could not be parsed as a task graph: {reason}")]
    ModelResponseInvalid { reason: String },
    #[error("planner mechanism not supported: {mechanism}")]
    MechanismNotSupported { mechanism: String },
}

#[async_trait]
pub trait TaskListInterpreter: Send + Sync {
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError>;

    async fn generate(
        &self,
        slug: &str,
        brief: &str,
        system_prompt_override: Option<&str>,
    ) -> Result<TaskGraph, InterpretError> {
        let _ = (slug, brief, system_prompt_override);
        Err(InterpretError::MechanismNotSupported {
            mechanism: "generate".into(),
        })
    }
}

/// Compatibility injection for constructors whose source projection is now
/// performed directly by the typed plan loader.
pub struct SourceProjectionUnavailable;

impl SourceProjectionUnavailable {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SourceProjectionUnavailable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskListInterpreter for SourceProjectionUnavailable {
    async fn interpret(
        &self,
        _slug: &str,
        _source_text: &str,
    ) -> Result<TaskGraph, InterpretError> {
        Err(InterpretError::MechanismNotSupported {
            mechanism: "typed plan loader required".into(),
        })
    }
}

pub const PLANNER_SYSTEM_PROMPT: &str = "Return only one JSON TaskGraph object.";
pub const PLANNER_GENERATE_SYSTEM_PROMPT: &str =
    "Return only one JSON TaskGraph object projected from the supplied typed plan context.";

pub struct ModelInterpreter {
    backend: Arc<dyn AgentBackend>,
    system_prompt: String,
    working_dir: std::path::PathBuf,
}

impl ModelInterpreter {
    pub fn new(backend: Arc<dyn AgentBackend>) -> Self {
        Self {
            backend,
            system_prompt: PLANNER_SYSTEM_PROMPT.into(),
            working_dir: std::path::PathBuf::from("/tmp"),
        }
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = dir.into();
        self
    }

    pub async fn generate(
        &self,
        slug: &str,
        brief: &str,
        system_prompt_override: Option<&str>,
    ) -> Result<TaskGraph, InterpretError> {
        let system_prompt = system_prompt_override.map_or_else(
            || PLANNER_GENERATE_SYSTEM_PROMPT.to_owned(),
            |extra| format!("{PLANNER_GENERATE_SYSTEM_PROMPT}\n\n{extra}"),
        );
        self.call(
            system_prompt,
            format!(
                "Project the typed plan context for `{slug}` and return only the JSON graph:\n\n{brief}"
            ),
        )
        .await
    }

    async fn call(
        &self,
        system_prompt: String,
        prompt: String,
    ) -> Result<TaskGraph, InterpretError> {
        let mut session = self
            .backend
            .spawn(SessionConfig {
                working_dir: self.working_dir.clone(),
                system_prompt,
                mode: None,
                model: None,
                effort: None,
                extra: None,
                task_id: None,
                run_id: String::new(),
            })
            .await?;
        let mut stream = session.prompt(Prompt::new(prompt)).await?;
        let mut raw = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                ResponseEvent::TextChunk { text } => raw.push_str(&text),
                ResponseEvent::TurnComplete { .. } => break,
                ResponseEvent::ThoughtChunk { .. }
                | ResponseEvent::ToolCall { .. }
                | ResponseEvent::ToolCallUpdate { .. }
                | ResponseEvent::CurrentModeUpdate { .. } => {}
            }
        }
        drop(stream);
        let _ = session.terminate().await;
        parse_model_response(&raw)
    }
}

#[async_trait]
impl TaskListInterpreter for ModelInterpreter {
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError> {
        self.call(
            self.system_prompt.clone(),
            format!("Project the typed source for `{slug}` and return only the JSON graph:\n\n{source_text}"),
        )
        .await
    }

    async fn generate(
        &self,
        slug: &str,
        brief: &str,
        system_prompt_override: Option<&str>,
    ) -> Result<TaskGraph, InterpretError> {
        ModelInterpreter::generate(self, slug, brief, system_prompt_override).await
    }
}

pub(crate) fn parse_model_response(raw: &str) -> Result<TaskGraph, InterpretError> {
    let json = crate::json::extract_json_object(raw).ok_or_else(|| {
        InterpretError::ModelResponseInvalid {
            reason: "no JSON object found in model response".into(),
        }
    })?;
    let graph: TaskGraph =
        serde_json::from_str(json).map_err(|error| InterpretError::ModelResponseInvalid {
            reason: format!("serde_json: {error}"),
        })?;
    graph.validate()?;
    Ok(graph)
}

pub fn build_planner_interpreter(
    mechanism: &crate::config::PlannerMechanism,
    backend: Option<Arc<dyn AgentBackend>>,
) -> Result<Arc<dyn TaskListInterpreter>, InterpretError> {
    match (mechanism, backend) {
        (crate::config::PlannerMechanism::OneShotAgent, Some(backend)) => {
            Ok(Arc::new(ModelInterpreter::new(backend)))
        }
        (crate::config::PlannerMechanism::OneShotAgent, None) => {
            Err(InterpretError::MechanismNotSupported {
                mechanism: "one-shot-agent without backend".into(),
            })
        }
        (crate::config::PlannerMechanism::DirectApi, _) => {
            Err(InterpretError::MechanismNotSupported {
                mechanism: "direct-api".into(),
            })
        }
    }
}

pub fn build_ingestion_interpreter(
    mechanism: &crate::config::PlannerMechanism,
    backend: Option<Arc<dyn AgentBackend>>,
) -> Result<Arc<dyn TaskListInterpreter>, InterpretError> {
    Ok(Arc::new(crate::dependency::EdgeInferrer::new(
        build_planner_interpreter(mechanism, backend)?,
    )))
}
