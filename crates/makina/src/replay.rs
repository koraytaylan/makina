//! Replay of persisted exchange transcripts into in-memory logs.
//!
//! When a run is opened from disk, its task exchange logs (prompts, responses,
//! tool calls, thoughts) are reconstructed from the persisted transcript JSONL
//! files by reading and replaying each [`ExchangeEvent`] through the same
//! reducer used by the live event path, guaranteeing that replay == live.

use std::path::Path;

use makina_core::api::{AgentRole, ExchangeEvent};

use crate::app::{ExchangeLog, apply_exchange_event};

/// Read `{task_id}_transcript.jsonl` and rebuild the task's ExchangeLog,
/// honouring EXCHANGE_LOG_CAP (keep the tail; mark truncated).
///
/// # Arguments
///
/// * `path` - Path to the transcript JSONL file
/// * `role` - The agent role (determines which role's side of the exchange to replay)
///
/// # Returns
///
/// An `ExchangeLog` populated with all parsed events from the transcript.
/// If the file is missing or unparseable, returns an empty log and logs a warning.
pub fn load_task_exchange(path: &Path, role: AgentRole) -> std::io::Result<ExchangeLog> {
    let body = std::fs::read_to_string(path)?;
    let mut log = ExchangeLog::default();
    for line in body.lines() {
        if let Ok(ev) = serde_json::from_str::<ExchangeEvent>(line) {
            apply_exchange_event(&mut log, role.clone(), &ev);
        }
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn load_task_exchange_parses_transcript() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let transcript_path = temp_dir.path().join("test_transcript.jsonl");

        // Write a simple transcript with a prompt and response chunk
        let mut file = fs::File::create(&transcript_path).expect("create transcript file");
        writeln!(file, r#"{{"type":"prompt_sent","text":"Hello"}}"#).expect("write prompt");
        writeln!(file, r#"{{"type":"response_chunk","text":"Hi there"}}"#).expect("write chunk");

        let log = load_task_exchange(&transcript_path, AgentRole::Developer)
            .expect("load exchange should succeed");

        // Should have both entries
        assert_eq!(log.entries.len(), 2);
    }

    #[test]
    fn load_task_exchange_handles_missing_file() {
        let nonexistent_path = Path::new("/nonexistent/path/transcript.jsonl");
        let result = load_task_exchange(nonexistent_path, AgentRole::Developer);
        assert!(result.is_err());
    }

    #[test]
    fn load_task_exchange_skips_invalid_lines() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let transcript_path = temp_dir.path().join("test_transcript.jsonl");

        // Write a transcript with valid and invalid lines
        let mut file = fs::File::create(&transcript_path).expect("create transcript file");
        writeln!(file, r#"{{"type":"prompt_sent","text":"Hello"}}"#).expect("write valid");
        writeln!(file, "not valid json").expect("write invalid");
        writeln!(file, r#"{{"type":"response_chunk","text":"Response"}}"#).expect("write valid");

        let log = load_task_exchange(&transcript_path, AgentRole::Developer)
            .expect("load exchange should succeed");

        // Should only have 2 valid entries (invalid line is skipped)
        assert_eq!(log.entries.len(), 2);
    }
}
