//! Utilities for combining command output streams.

/// Combine a command's stdout and stderr into a single string for feedback.
///
/// Both streams are decoded lossily (command output is human text, not guaranteed
/// UTF-8). stdout is shown first, then stderr under a label, so the agent sees
/// the full picture. Empty streams are omitted to keep the blob compact.
pub(crate) fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_output_labels_stderr() {
        assert_eq!(combine_output(b"out", b""), "out");
        assert_eq!(combine_output(b"", b"err"), "stderr:\nerr");
        assert_eq!(combine_output(b"out", b"err"), "out\nstderr:\nerr");
        assert_eq!(combine_output(b"", b""), "");
    }
}
