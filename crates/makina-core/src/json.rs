//! Shared JSON extraction helpers.
//!
//! Provides `extract_json_object`, used by both the planner interpreter and the
//! reviewer role parser to robustly pull the outermost JSON object out of model
//! text that may contain prose or Markdown fences.

/// Find and return the first outermost `{ … }` JSON object substring in `text`.
///
/// Strips leading/trailing prose and ` ```json … ``` ` code fences. Handles
/// nested braces by tracking brace depth, and correctly ignores `{` / `}` inside
/// JSON string literals. Returns `None` if no `{` is found.
pub(crate) fn extract_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape_next = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start
                {
                    return Some(&text[s..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_finds_bare_object() {
        let text = r#"{"slug":"x","tasks":[]}"#;
        let extracted = extract_json_object(text).unwrap();
        assert_eq!(extracted, text);
    }

    #[test]
    fn extract_json_object_strips_leading_prose() {
        let text = r#"Here you go: {"slug":"x","tasks":[]}"#;
        let extracted = extract_json_object(text).unwrap();
        assert_eq!(extracted, r#"{"slug":"x","tasks":[]}"#);
    }

    #[test]
    fn extract_json_object_handles_nested_braces() {
        let text = r#"{"slug":"x","tasks":[{"id":"a","title":"T"}]}"#;
        let extracted = extract_json_object(text).unwrap();
        assert_eq!(extracted, text);
    }

    #[test]
    fn extract_json_object_returns_none_for_no_object() {
        let text = "No JSON here at all.";
        assert!(extract_json_object(text).is_none());
    }

    #[test]
    fn extract_json_object_handles_string_containing_braces() {
        // A JSON string value containing `{` and `}` must not confuse the parser.
        let text = r#"{"slug":"x{y}","tasks":[]}"#;
        let extracted = extract_json_object(text).unwrap();
        assert_eq!(extracted, text);
    }
}
