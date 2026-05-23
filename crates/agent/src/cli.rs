use anyhow::Context;
use uuid::Uuid;

pub(crate) const DEFAULT_BITRATE: u32 = 50_000; // 50 Mbps -- LAN default
pub(crate) const DEFAULT_FRAMERATE: u32 = 120; // 120fps

#[derive(Debug)]
pub(crate) struct Args {
    pub display: String,
    pub server_url: String,
    pub session_id: Uuid,
    pub agent_token: Option<String>,
    pub tls_cert_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate: u32,
    pub encoder: Option<String>,
    pub max_width: u32,
    pub max_height: u32,
    pub gpu_driver: String,
    pub display_start: u32,
}

/// Outcome of CLI argument parsing.
///
/// Split out so the parser is unit-testable: the public `parse_args()` wraps
/// this in handling for `--version` and `--help` which terminate the process,
/// but the underlying parse logic in `parse_args_from` is side-effect-free
/// (no `std::process::exit`, no `println!`).
#[derive(Debug)]
pub(crate) enum ArgsOutcome {
    /// Continue startup with these settings.
    Run(Args),
    /// Print the version banner, then exit successfully.
    PrintVersion,
    /// Print help text, then exit successfully.
    PrintHelp,
}

/// Parse arguments from an iterator (typically `std::env::args()`).
///
/// Side-effect free: returns `ArgsOutcome` instead of calling
/// `std::process::exit`. The caller is responsible for handling the
/// `PrintVersion` / `PrintHelp` variants.
pub(crate) fn parse_args_from<I, S>(args: I) -> anyhow::Result<ArgsOutcome>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();

    let mut display = ":0".to_string();
    let mut server_url = String::new();
    let mut session_id = None;
    let mut agent_token = None;
    let mut tls_cert_path = None;
    let mut width: u32 = 1920;
    let mut height: u32 = 1080;
    let mut framerate: u32 = DEFAULT_FRAMERATE;
    let mut bitrate: u32 = DEFAULT_BITRATE;
    let mut encoder: Option<String> = None;
    let mut max_width: u32 = 3840;
    let mut max_height: u32 = 2160;
    let mut gpu_driver = "auto".to_string();
    let mut display_start: u32 = 10;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-V" | "--version" => return Ok(ArgsOutcome::PrintVersion),
            "-h" | "--help" => return Ok(ArgsOutcome::PrintHelp),
            "--display" => {
                i += 1;
                display = args.get(i).context("Missing --display value")?.clone();
            }
            "--server-url" => {
                i += 1;
                server_url = args.get(i).context("Missing --server-url value")?.clone();
            }
            "--session-id" => {
                i += 1;
                session_id = Some(
                    args.get(i)
                        .context("Missing --session-id value")?
                        .parse::<Uuid>()
                        .context("Invalid session-id UUID")?,
                );
            }
            "--agent-token" => {
                // Legacy CLI support (prefer BEAM_AGENT_TOKEN env var)
                i += 1;
                agent_token = Some(args.get(i).context("Missing --agent-token value")?.clone());
            }
            "--tls-cert" => {
                i += 1;
                tls_cert_path = Some(args.get(i).context("Missing --tls-cert value")?.clone());
            }
            "--width" => {
                i += 1;
                width = args
                    .get(i)
                    .context("Missing --width value")?
                    .parse()
                    .context("Invalid --width value")?;
            }
            "--height" => {
                i += 1;
                height = args
                    .get(i)
                    .context("Missing --height value")?
                    .parse()
                    .context("Invalid --height value")?;
            }
            "--framerate" => {
                i += 1;
                framerate = args
                    .get(i)
                    .context("Missing --framerate value")?
                    .parse()
                    .context("Invalid --framerate value")?;
            }
            "--bitrate" => {
                i += 1;
                bitrate = args
                    .get(i)
                    .context("Missing --bitrate value")?
                    .parse()
                    .context("Invalid --bitrate value")?;
            }
            "--encoder" => {
                i += 1;
                encoder = Some(args.get(i).context("Missing --encoder value")?.clone());
            }
            "--max-width" => {
                i += 1;
                max_width = args
                    .get(i)
                    .context("Missing --max-width value")?
                    .parse()
                    .context("Invalid --max-width value")?;
            }
            "--max-height" => {
                i += 1;
                max_height = args
                    .get(i)
                    .context("Missing --max-height value")?
                    .parse()
                    .context("Invalid --max-height value")?;
            }
            "--gpu-driver" => {
                i += 1;
                gpu_driver = args.get(i).context("Missing --gpu-driver value")?.clone();
            }
            "--display-start" => {
                i += 1;
                display_start = args
                    .get(i)
                    .context("Missing --display-start value")?
                    .parse()
                    .context("Invalid --display-start value")?;
            }
            other => anyhow::bail!("Unknown argument: {other}"),
        }
        i += 1;
    }

    // Prefer env var for agent token (CLI args are visible in /proc)
    if agent_token.is_none() {
        agent_token = std::env::var("BEAM_AGENT_TOKEN").ok();
    }

    Ok(ArgsOutcome::Run(Args {
        display,
        server_url,
        session_id: session_id.context("--session-id is required")?,
        agent_token,
        tls_cert_path,
        width,
        height,
        framerate,
        bitrate,
        encoder,
        max_width,
        max_height,
        gpu_driver,
        display_start,
    }))
}

pub(crate) fn parse_args() -> anyhow::Result<Args> {
    let args: Vec<String> = std::env::args().collect();
    match parse_args_from(args)? {
        ArgsOutcome::Run(args) => Ok(args),
        ArgsOutcome::PrintVersion => {
            println!("{}", version_line(env!("CARGO_PKG_VERSION")));
            std::process::exit(0);
        }
        ArgsOutcome::PrintHelp => {
            print_help();
            std::process::exit(0);
        }
    }
}

fn print_help() {
    print!("{}", help_text());
}

/// Build the agent --help output. Pure helper so the format can be
/// unit-tested without process I/O.
pub(crate) fn help_text() -> String {
    let mut s = String::new();
    s.push_str("beam-agent - Beam Remote Desktop capture agent\n");
    s.push('\n');
    s.push_str("USAGE:\n");
    s.push_str("    beam-agent [OPTIONS]\n");
    s.push('\n');
    s.push_str("OPTIONS:\n");
    s.push_str("    --display <DISPLAY>          X11 display [default: :0]\n");
    s.push_str("    --server-url <URL>           Signaling server WebSocket URL\n");
    s.push_str("    --session-id <UUID>          Session identifier (required)\n");
    s.push_str(
        "    --agent-token <TOKEN>        Agent authentication token (prefer BEAM_AGENT_TOKEN env)\n",
    );
    s.push_str("    --tls-cert <PATH>            TLS certificate to pin for server connection\n");
    s.push_str("    --width <PIXELS>             Initial display width [default: 1920]\n");
    s.push_str("    --height <PIXELS>            Initial display height [default: 1080]\n");
    s.push_str("    --framerate <FPS>            Target framerate [default: 120]\n");
    s.push_str("    --bitrate <KBPS>             Initial video bitrate [default: 100000]\n");
    s.push_str("    --encoder <NAME>             Force encoder (nvh264enc, vah264enc, x264enc)\n");
    s.push_str("    --max-width <PIXELS>         Maximum resize width [default: 3840]\n");
    s.push_str("    --max-height <PIXELS>        Maximum resize height [default: 2160]\n");
    s.push_str(
        "    --gpu-driver <MODE>          GPU driver: auto, nvidia, dummy [default: auto]\n",
    );
    s.push_str("    --display-start <N>          Starting display number [default: 10]\n");
    s.push_str("    -V, --version                Print version and exit\n");
    s.push_str("    -h, --help                   Print this help and exit\n");
    s
}

/// Build the agent --version output. Pure helper. The production code
/// reads CARGO_PKG_VERSION at compile time; here the caller passes the
/// version explicitly so tests can verify the prefix.
pub(crate) fn version_line(version: &str) -> String {
    format!("beam-agent {version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<ArgsOutcome> {
        // SAFETY: tests share process env. Clear BEAM_AGENT_TOKEN so tests are
        // deterministic regardless of run order.
        // SAFETY: clearing an env var is safe and only affects this process.
        unsafe { std::env::remove_var("BEAM_AGENT_TOKEN") };
        let mut full = vec!["beam-agent".to_string()];
        full.extend(args.iter().map(|s| s.to_string()));
        parse_args_from(full)
    }

    fn parsed_args(args: &[&str]) -> anyhow::Result<Args> {
        match parse(args)? {
            ArgsOutcome::Run(a) => Ok(a),
            other => anyhow::bail!("Expected Run, got {:?}", other),
        }
    }

    #[test]
    fn defaults_with_only_session_id() {
        let id = Uuid::new_v4();
        let a = parsed_args(&["--session-id", &id.to_string()]).unwrap();
        assert_eq!(a.display, ":0");
        assert_eq!(a.server_url, "");
        assert_eq!(a.session_id, id);
        assert!(a.agent_token.is_none());
        assert_eq!(a.width, 1920);
        assert_eq!(a.height, 1080);
        assert_eq!(a.framerate, DEFAULT_FRAMERATE);
        assert_eq!(a.bitrate, DEFAULT_BITRATE);
        assert_eq!(a.max_width, 3840);
        assert_eq!(a.max_height, 2160);
        assert_eq!(a.gpu_driver, "auto");
        assert_eq!(a.display_start, 10);
        assert!(a.encoder.is_none());
        assert!(a.tls_cert_path.is_none());
    }

    #[test]
    fn missing_session_id_returns_err() {
        let result = parse(&[]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("--session-id is required")
        );
    }

    #[test]
    fn invalid_session_id_returns_err() {
        let result = parse(&["--session-id", "not-a-uuid"]);
        assert!(result.is_err());
    }

    #[test]
    fn version_short_returns_print_version() {
        let outcome = parse(&["-V"]).unwrap();
        assert!(matches!(outcome, ArgsOutcome::PrintVersion));
    }

    #[test]
    fn version_long_returns_print_version() {
        let outcome = parse(&["--version"]).unwrap();
        assert!(matches!(outcome, ArgsOutcome::PrintVersion));
    }

    #[test]
    fn help_short_returns_print_help() {
        let outcome = parse(&["-h"]).unwrap();
        assert!(matches!(outcome, ArgsOutcome::PrintHelp));
    }

    #[test]
    fn help_long_returns_print_help() {
        let outcome = parse(&["--help"]).unwrap();
        assert!(matches!(outcome, ArgsOutcome::PrintHelp));
    }

    #[test]
    fn version_short_circuits_before_session_id_required() {
        // --version should bypass --session-id requirement
        let outcome = parse(&["--version"]).unwrap();
        assert!(matches!(outcome, ArgsOutcome::PrintVersion));
    }

    #[test]
    fn display_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&["--display", ":10", "--session-id", &id.to_string()]).unwrap();
        assert_eq!(a.display, ":10");
    }

    #[test]
    fn server_url_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--server-url",
            "wss://example.test/ws",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.server_url, "wss://example.test/ws");
    }

    #[test]
    fn agent_token_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--agent-token",
            "secret-token-value",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.agent_token.as_deref(), Some("secret-token-value"));
    }

    #[test]
    fn tls_cert_path_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--tls-cert",
            "/etc/beam/cert.pem",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.tls_cert_path.as_deref(), Some("/etc/beam/cert.pem"));
    }

    #[test]
    fn width_height_overrides() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--width",
            "2560",
            "--height",
            "1440",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.width, 2560);
        assert_eq!(a.height, 1440);
    }

    #[test]
    fn framerate_bitrate_overrides() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--framerate",
            "60",
            "--bitrate",
            "10000",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.framerate, 60);
        assert_eq!(a.bitrate, 10000);
    }

    #[test]
    fn encoder_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&["--encoder", "x264enc", "--session-id", &id.to_string()]).unwrap();
        assert_eq!(a.encoder.as_deref(), Some("x264enc"));
    }

    #[test]
    fn max_dimensions_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&[
            "--max-width",
            "7680",
            "--max-height",
            "4320",
            "--session-id",
            &id.to_string(),
        ])
        .unwrap();
        assert_eq!(a.max_width, 7680);
        assert_eq!(a.max_height, 4320);
    }

    #[test]
    fn gpu_driver_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&["--gpu-driver", "nvidia", "--session-id", &id.to_string()]).unwrap();
        assert_eq!(a.gpu_driver, "nvidia");
    }

    #[test]
    fn display_start_override() {
        let id = Uuid::new_v4();
        let a = parsed_args(&["--display-start", "42", "--session-id", &id.to_string()]).unwrap();
        assert_eq!(a.display_start, 42);
    }

    #[test]
    fn unknown_argument_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--bogus", "--session-id", &id.to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown argument"));
    }

    #[test]
    fn missing_value_for_display_returns_err() {
        let result = parse(&["--display"]);
        assert!(result.is_err());
    }

    #[test]
    fn missing_value_for_width_returns_err() {
        let result = parse(&["--width"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_width_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--width", "not-a-number", "--session-id", &id.to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("--width"));
    }

    #[test]
    fn invalid_height_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--height", "not-a-number", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_framerate_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--framerate", "x", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_bitrate_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--bitrate", "abc", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_max_width_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--max-width", "x", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_max_height_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--max-height", "x", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_display_start_value_returns_err() {
        let id = Uuid::new_v4();
        let result = parse(&["--display-start", "x", "--session-id", &id.to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn defaults_are_sensible_constants() {
        // Document and lock the default constants.
        assert_eq!(DEFAULT_BITRATE, 50_000);
        assert_eq!(DEFAULT_FRAMERATE, 120);
    }

    // --- help_text ---

    #[test]
    fn help_text_starts_with_program_name() {
        let h = help_text();
        assert!(h.starts_with("beam-agent"));
    }

    #[test]
    fn help_text_lists_usage_section() {
        let h = help_text();
        assert!(h.contains("USAGE:"));
        assert!(h.contains("beam-agent [OPTIONS]"));
    }

    #[test]
    fn help_text_lists_options_section() {
        let h = help_text();
        assert!(h.contains("OPTIONS:"));
        // Spot-check every major flag is documented.
        for flag in [
            "--display",
            "--server-url",
            "--session-id",
            "--agent-token",
            "--tls-cert",
            "--width",
            "--height",
            "--framerate",
            "--bitrate",
            "--encoder",
            "--max-width",
            "--max-height",
            "--gpu-driver",
            "--display-start",
            "-V, --version",
            "-h, --help",
        ] {
            assert!(h.contains(flag), "Help text is missing {flag}");
        }
    }

    #[test]
    fn help_text_documents_defaults() {
        let h = help_text();
        // Spot-check default values are visible to operators.
        assert!(h.contains("[default: :0]"));
        assert!(h.contains("[default: 1920]"));
        assert!(h.contains("[default: 1080]"));
        assert!(h.contains("[default: 120]"));
        assert!(h.contains("[default: 100000]"));
        assert!(h.contains("[default: 3840]"));
        assert!(h.contains("[default: 2160]"));
        assert!(h.contains("[default: auto]"));
        assert!(h.contains("[default: 10]"));
    }

    #[test]
    fn help_text_documents_required_session_id() {
        let h = help_text();
        assert!(h.contains("(required)"));
    }

    #[test]
    fn help_text_mentions_env_var_for_agent_token() {
        let h = help_text();
        assert!(h.contains("BEAM_AGENT_TOKEN"));
    }

    #[test]
    fn help_text_ends_with_newline() {
        // Operator-friendly: terminal stays aligned.
        let h = help_text();
        assert!(h.ends_with('\n'));
    }

    // --- version_line ---

    #[test]
    fn version_line_includes_version() {
        let v = version_line("0.3.99");
        assert_eq!(v, "beam-agent 0.3.99");
    }

    #[test]
    fn version_line_includes_dev_suffix() {
        let v = version_line("0.4.0-dev");
        assert!(v.contains("0.4.0-dev"));
    }

    #[test]
    fn version_line_handles_empty_version() {
        // Defensive — pkg version is always set, but the formatter should
        // not panic on empty.
        let v = version_line("");
        assert_eq!(v, "beam-agent ");
    }
}
