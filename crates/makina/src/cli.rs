//! CLI argument parsing and text renderers for the makina binary.
//!
//! Provides a pure parser (no env reads) for dispatching on command-line flags,
//! along with help, version, and doctor report renderers.

pub const AVAILABLE_TEMPLATES: &[&str] = &["todo"];

#[derive(Debug, PartialEq, Eq)]
pub enum CliAction {
    LaunchTui,
    ShowHelp,
    ShowVersion,
    RunDoctor,
    Create { path: String, template: String },
    CreateError(String),
    Unknown(String),
}

/// Parse argv already stripped of argv[0]. A bare invocation launches the TUI.
pub fn parse_args(args: &[String]) -> CliAction {
    match args.first().map(String::as_str) {
        None => CliAction::LaunchTui,
        Some("-h") | Some("--help") => CliAction::ShowHelp,
        Some("-V") | Some("--version") => CliAction::ShowVersion,
        Some("--doctor") => CliAction::RunDoctor,
        Some("create") => parse_create(&args[1..]),
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}

fn parse_create(rest: &[String]) -> CliAction {
    let mut path: Option<String> = None;
    let mut template = String::from("todo");
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--template" => match rest.get(i + 1) {
                Some(t) => {
                    template = t.clone();
                    i += 2;
                }
                None => return CliAction::CreateError("--template requires a value".to_string()),
            },
            other if !other.starts_with('-') && path.is_none() => {
                path = Some(other.to_string());
                i += 1;
            }
            other => return CliAction::CreateError(format!("unexpected argument: {other}")),
        }
    }
    let Some(path) = path else {
        return CliAction::CreateError("makina create requires a target <path>".to_string());
    };
    if !AVAILABLE_TEMPLATES.contains(&template.as_str()) {
        return CliAction::CreateError(format!(
            "unknown template '{template}'; available templates: {}",
            AVAILABLE_TEMPLATES.join(", ")
        ));
    }
    CliAction::Create { path, template }
}

/// Return the help text listing usage, config file locations, and available flags.
pub fn help_text() -> String {
    String::from(
        "makina — multi-agent software-factory orchestrator\n\
         \n\
         USAGE:\n  \
         makina [OPTIONS]\n\
         \n\
         OPTIONS:\n  \
         -h, --help\n    \
         Show this help message\n  \
         -V, --version\n    \
         Show version and git commit hash\n  \
         --doctor\n    \
         Run a headless preflight check (exit non-zero if no backend is configured or detected)\n\
         \n\
         SUBCOMMANDS:\n  \
         create <path> [--template <name>]\n    \
         Scaffold a new, runnable Makina project at <path> (default template: todo)\n    \
         Available templates: todo\n\
         \n\
         CONFIGURATION:\n  \
         Global config: ~/.makina/config.toml\n  \
         Project config: .makina/config.toml\n\
         \n\
         For more information, see README § Configure.\n",
    )
}

/// Return the version text, including git commit hash if available.
pub fn version_text() -> String {
    match option_env!("MAKINA_GIT_SHA") {
        Some(sha) => format!("makina {} ({sha})", env!("CARGO_PKG_VERSION")),
        None => format!("makina {}", env!("CARGO_PKG_VERSION")),
    }
}

/// Render the headless `--doctor` report and its process exit code.
///
/// A backend is resolvable when the config already loads OR a KNOWN_AGENTS
/// binary is detected on PATH; otherwise exit non-zero so scripts can gate on it.
pub fn render_doctor_report(detected: Option<&str>, config_loaded: bool) -> (String, i32) {
    let mut out = String::from("makina doctor\n");
    match detected {
        Some(agent) => out.push_str(&format!("  detected agent on PATH: {agent}\n")),
        None => out.push_str("  detected agent on PATH: none\n"),
    }
    out.push_str(&format!("  config loads: {config_loaded}\n"));
    let ok = config_loaded || detected.is_some();
    out.push_str(if ok {
        "  status: OK\n"
    } else {
        "  status: FAIL — no agent backend configured or detected\n"
    });
    (out, if ok { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_dispatches_each_flag() {
        assert_eq!(parse_args(&[]), CliAction::LaunchTui);
        assert_eq!(parse_args(&["--help".into()]), CliAction::ShowHelp);
        assert_eq!(parse_args(&["-h".into()]), CliAction::ShowHelp);
        assert_eq!(parse_args(&["--version".into()]), CliAction::ShowVersion);
        assert_eq!(parse_args(&["-V".into()]), CliAction::ShowVersion);
        assert_eq!(parse_args(&["--doctor".into()]), CliAction::RunDoctor);
        assert_eq!(
            parse_args(&["--nope".into()]),
            CliAction::Unknown("--nope".into())
        );
    }

    #[test]
    fn version_text_starts_with_pkg_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn doctor_report_exit_codes() {
        assert_eq!(render_doctor_report(Some("gemini"), false).1, 0);
        assert_eq!(render_doctor_report(None, true).1, 0);
        assert_eq!(render_doctor_report(None, false).1, 1);
    }

    #[test]
    fn parse_args_handles_create_subcommand() {
        assert_eq!(
            parse_args(&["create".into(), "/tmp/x".into()]),
            CliAction::Create {
                path: "/tmp/x".into(),
                template: "todo".into()
            }
        );
        assert_eq!(
            parse_args(&[
                "create".into(),
                "/tmp/x".into(),
                "--template".into(),
                "todo".into()
            ]),
            CliAction::Create {
                path: "/tmp/x".into(),
                template: "todo".into()
            }
        );
        match parse_args(&["create".into()]) {
            CliAction::CreateError(msg) => assert!(msg.contains("path")),
            other => panic!("expected CreateError, got {other:?}"),
        }
        match parse_args(&[
            "create".into(),
            "/tmp/x".into(),
            "--template".into(),
            "nope".into(),
        ]) {
            CliAction::CreateError(msg) => assert!(msg.contains("todo")),
            other => panic!("expected CreateError, got {other:?}"),
        }
    }
}
