//! Guard test: `makina-acp` must never write to stdout/stderr directly.
//!
//! The ACP backend runs *while the TUI's ratatui frame is live* (it drives the
//! agent subprocess during a run). Any `eprintln!`/`println!` here — e.g.
//! forwarding the agent's stderr, or logging an ACP protocol anomaly — is
//! written straight to the terminal, lands at column 0, and scrolls/overwrites
//! the alternate-screen frame ("`[acp-agent] …` dumped randomly over the UI").
//!
//! All diagnostics MUST instead flow through `tracing`, so plan-0003's
//! subscriber routes them to the per-run/per-task log file and the TUI error
//! pane (never the raw terminal). This test pins that invariant: zero raw print
//! macros in any `makina-acp/src` file (doc-comment examples excluded).

use std::path::{Path, PathBuf};

/// Counts `eprintln!`/`println!`/`eprint!`/`print!` macro invocations in a
/// source string, skipping `//` line comments and `//!`/`///` doc comments so
/// prose / doc examples that mention the macros do not false-positive.
fn count_print_macros(source: &str) -> usize {
    let mut count = 0;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        // Match each macro only at an identifier boundary so `eprintln!` is not
        // also counted as `println!` (it contains that substring), etc.
        for macro_name in ["eprintln!", "println!", "eprint!", "print!"] {
            let mut from = 0;
            while let Some(rel) = line[from..].find(macro_name) {
                let start = from + rel;
                let preceded_by_ident = start
                    .checked_sub(1)
                    .map(|i| {
                        let b = line.as_bytes()[i];
                        b.is_ascii_alphanumeric() || b == b'_'
                    })
                    .unwrap_or(false);
                if !preceded_by_ident {
                    count += 1;
                }
                from = start + macro_name.len();
            }
        }
    }
    count
}

/// Recursively collect every `.rs` file under `dir`.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_raw_terminal_writes_in_makina_acp_src() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "expected to find source files under {src:?}"
    );

    let mut offenders = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read source file");
        let n = count_print_macros(&source);
        if n > 0 {
            offenders.push(format!("{}: {n} raw print macro(s)", file.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "makina-acp must route all diagnostics through `tracing`, never raw \
         stdout/stderr (it runs while the TUI frame is live). Offending files:\n{}",
        offenders.join("\n")
    );
}
