mod auth;
mod client_metrics;
mod config;
mod session;
mod signaling;
mod tls;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::session::SessionManager;
use crate::web::AppState;

/// What `parse_args_from` requests of the caller.
///
/// Split out so the argument parser is testable (no `std::process::exit`
/// inside, no `println!` side effects in the hot path).
#[derive(Debug, PartialEq, Eq)]
enum ArgsOutcome {
    /// Continue startup with these settings.
    Run {
        config_path: PathBuf,
        port_override: Option<u16>,
    },
    /// Print the version banner, then exit successfully.
    PrintVersion,
    /// Print help text, then exit successfully.
    PrintHelp,
}

fn parse_args_from<I, S>(args: I) -> ArgsOutcome
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut config_path = PathBuf::from("./config/beam.toml");
    let mut port_override = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-V" | "--version" => return ArgsOutcome::PrintVersion,
            "-h" | "--help" => return ArgsOutcome::PrintHelp,
            "--config" | "-c" if i + 1 < args.len() => {
                config_path = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            "--port" | "-p" if i + 1 < args.len() => {
                port_override = args[i + 1].parse().ok();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    ArgsOutcome::Run {
        config_path,
        port_override,
    }
}

/// Parse a `bind:port` pair into a [`SocketAddr`]. Split out so the parsing
/// branch can be unit-tested without binding a real port.
fn parse_bind_addr(bind: &str, port: u16) -> Result<SocketAddr> {
    format!("{bind}:{port}")
        .parse()
        .context("Invalid bind address")
}

/// Load a persisted JWT secret from `secret_path`, returning `None` if the
/// file is missing or empty. Split out so the persistence branches can be
/// unit-tested with a tempfile fixture.
fn load_persisted_jwt_secret(secret_path: &std::path::Path) -> Option<String> {
    match std::fs::read_to_string(secret_path) {
        Ok(existing) => {
            let trimmed = existing.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

/// Classify the validation `issues` returned by [`BeamConfig::validate`].
/// Returns true if any issue is a hard error (must abort) vs a warning.
/// Split out for unit-testing — the production path logs warnings + errors
/// and conditionally exits, but the classification itself is a pure check.
fn has_validation_errors(issues: &[String]) -> bool {
    issues.iter().any(|i| i.starts_with("ERROR:"))
}

/// Build the warning message shown when a config has at least one ERROR.
/// Pure formatter, used by main() before [`std::process::exit`].
fn config_validation_summary(issues: &[String]) -> String {
    format!(
        "Configuration has {} issue(s). Fix the ERROR(s) above and restart.",
        issues.len()
    )
}

/// Persist a newly-generated JWT secret to `secret_path` (0600 file mode).
/// Returns `Err` if directory creation or file write fails. The caller logs
/// and falls back to in-memory only on error.
fn persist_jwt_secret(secret_path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = secret_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(secret_path)?;
    f.write_all(secret.as_bytes())?;
    Ok(())
}

/// Outcome of resolving the JWT secret for a new server instance. Split out
/// so the persistence/generation flow can be unit-tested without spinning up
/// `main()`. Captures whether the secret was loaded, generated + persisted,
/// or fell back to ephemeral generation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JwtSecretSource {
    /// Successfully loaded an existing secret from disk.
    LoadedFromDisk,
    /// Generated a new secret and persisted it to disk.
    GeneratedAndPersisted,
    /// Generated a new secret but couldn't persist it (e.g. disk full,
    /// permission denied). The secret is held only in memory; surviving
    /// a restart means re-issuing every JWT.
    GeneratedEphemeral(String),
}

/// Classify each `config.validate()` issue as either `ERROR:` or `WARN:`
/// and return the pair `(error_count, warning_count)`. Used by the
/// startup banner and by tests asserting the counter behavior.
pub(crate) fn count_validation_severities(issues: &[String]) -> (usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    for issue in issues {
        if issue.starts_with("ERROR:") {
            errors += 1;
        } else {
            warnings += 1;
        }
    }
    (errors, warnings)
}

/// Build the warning message shown when the PAM config is missing. Pure
/// helper so the warning text is unit-testable (operators rely on it
/// when troubleshooting "agent sessions don't start").
pub(crate) fn pam_missing_warning() -> &'static str {
    "PAM config /etc/pam.d/beam not found — agent sessions will fail to start. \
     Install the beam package or copy packaging/pam.d/beam to /etc/pam.d/beam."
}

/// Build the warning message shown when the configured `web_root`
/// directory doesn't exist on disk. Pure helper so the message can be
/// unit-tested.
pub(crate) fn web_root_missing_warning(web_root: &str) -> String {
    format!(
        "Web root '{}' does not exist — the UI will not load. \
         Build with 'make build-web' or set server.web_root in the config.",
        web_root
    )
}

/// Decide whether a `web_root` path is a usable directory. Pure helper
/// that short-circuits when the path is empty (defensive: an empty
/// web_root means the operator hasn't set it).
pub(crate) fn web_root_is_usable(web_root: &str) -> bool {
    if web_root.is_empty() {
        return false;
    }
    std::path::Path::new(web_root).is_dir()
}

/// Build the startup banner line with the bound address. Pure helper —
/// the production main() logs three lines around this; here we just
/// pin the formatting in one testable place.
pub(crate) fn startup_banner_line(bind_addr: &SocketAddr) -> String {
    format!("Listening on https://{bind_addr}")
}

/// Resolve the JWT secret: load from disk if present, else generate +
/// persist. Returns the secret plus a `JwtSecretSource` enum describing
/// which branch fired so the caller can log appropriately and tests can
/// assert behavior.
pub(crate) fn resolve_jwt_secret(
    configured: Option<String>,
    secret_path: &std::path::Path,
    generator: impl FnOnce() -> String,
) -> (String, JwtSecretSource) {
    if let Some(s) = configured {
        // Config-provided overrides everything — treat as loaded.
        return (s, JwtSecretSource::LoadedFromDisk);
    }
    if let Some(existing) = load_persisted_jwt_secret(secret_path) {
        return (existing, JwtSecretSource::LoadedFromDisk);
    }
    let secret = generator();
    match persist_jwt_secret(secret_path, &secret) {
        Ok(()) => (secret, JwtSecretSource::GeneratedAndPersisted),
        Err(e) => {
            let err_msg = e.to_string();
            (secret, JwtSecretSource::GeneratedEphemeral(err_msg))
        }
    }
}

fn parse_args() -> (PathBuf, Option<u16>) {
    let args: Vec<String> = std::env::args().collect();
    match parse_args_from(args) {
        ArgsOutcome::Run {
            config_path,
            port_override,
        } => (config_path, port_override),
        ArgsOutcome::PrintVersion => {
            println!("beam-server {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        ArgsOutcome::PrintHelp => {
            println!("beam-server - Beam Remote Desktop signaling server");
            println!();
            println!("USAGE:");
            println!("    beam-server [OPTIONS]");
            println!();
            println!("OPTIONS:");
            println!("    -c, --config <PATH>    Configuration file [default: ./config/beam.toml]");
            println!("    -p, --port <PORT>      Override server port");
            println!("    -V, --version          Print version and exit");
            println!("    -h, --help             Print this help and exit");
            std::process::exit(0);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (config_path, port_override) = parse_args();

    // Load configuration
    let mut config = config::load_config(&config_path)?;
    if let Some(p) = port_override {
        config.server.port = p;
    }
    // Validate configuration semantics
    if let Err(issues) = config.validate() {
        let has_errors = has_validation_errors(&issues);
        for issue in &issues {
            if issue.starts_with("ERROR:") {
                tracing::error!("{}", issue);
            } else {
                tracing::warn!("{}", issue);
            }
        }
        if has_errors {
            tracing::error!("{}", config_validation_summary(&issues));
            std::process::exit(1);
        }
    }

    // Validate PAM config exists (required for systemd-logind session registration)
    if !std::path::Path::new("/etc/pam.d/beam").exists() {
        tracing::warn!("{}", pam_missing_warning());
    }

    // Validate web root exists so we don't silently serve 404
    if !web_root_is_usable(&config.server.web_root) {
        tracing::warn!("{}", web_root_missing_warning(&config.server.web_root));
    }

    let port = config.server.port;
    let bind_addr: SocketAddr = parse_bind_addr(&config.server.bind, port)?;

    // Build TLS config
    let tls_result = tls::build_tls_config(
        config.server.tls_cert.as_deref(),
        config.server.tls_key.as_deref(),
    )?;
    let tls_acceptor = tls::make_acceptor(tls_result.config);
    let tls_cert_path = tls_result.cert_pem_path;

    // JWT secret — persist to /var/lib/beam/jwt_secret so tokens survive restarts
    let secret_path = std::path::Path::new("/var/lib/beam/jwt_secret");
    let (jwt_secret, source) = resolve_jwt_secret(
        config.server.jwt_secret.clone(),
        secret_path,
        auth::generate_secret,
    );
    match &source {
        JwtSecretSource::LoadedFromDisk => {
            tracing::info!("Loaded JWT secret from {}", secret_path.display());
        }
        JwtSecretSource::GeneratedAndPersisted => {
            tracing::info!("Persisted JWT secret to {}", secret_path.display());
        }
        JwtSecretSource::GeneratedEphemeral(err) => {
            tracing::warn!("Failed to persist JWT secret: {err}");
        }
    }

    // Session manager
    let session_manager = SessionManager::new(
        config.session.display_start,
        config.session.default_width,
        config.session.default_height,
        Some(tls_cert_path),
        config.video.clone(),
        config.session.gpu_driver.clone(),
    );

    // Build app state and router
    let state = Arc::new(AppState {
        config,
        session_manager,
        channels: signaling::new_channel_registry(),
        jwt_secret,
        login_limiter: web::LoginRateLimiter::new(5, 60), // 5 attempts per username per 60s
        ip_limiter: web::LoginRateLimiter::new(20, 60),   // 20 attempts per IP per 60s
        release_limiter: web::LoginRateLimiter::new(10, 60), // 10 release attempts per IP per 60s
        started_at: std::time::Instant::now(),
        metrics_logins_attempted: std::sync::atomic::AtomicU64::new(0),
        metrics_logins_failed: std::sync::atomic::AtomicU64::new(0),
        metrics_agent_restarts: std::sync::atomic::AtomicU64::new(0),
        client_metrics: Arc::new(client_metrics::ClientMetricsStore::default()),
    });

    // Restore sessions from previous graceful shutdown
    let restored = state.session_manager.restore_sessions().await;
    for session_id in &restored {
        signaling::get_or_create_channel(&state.channels, *session_id).await;
        web::spawn_agent_monitor(Arc::clone(&state), *session_id).await;
    }
    if !restored.is_empty() {
        tracing::info!(
            "Restored {} sessions from previous shutdown",
            restored.len()
        );
    }

    if state.config.server.admin_users.is_empty() {
        tracing::info!("Admin panel disabled (no admin_users configured in beam.toml)");
    }

    let app = web::build_router(Arc::clone(&state))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        path = %request.uri().path(),
                        request_id = %request_id,
                    )
                })
                .on_request(|_request: &axum::http::Request<_>, _span: &tracing::Span| {
                    tracing::event!(Level::INFO, "started");
                })
                .on_response(
                    |response: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        tracing::event!(
                            Level::INFO,
                            status = %response.status().as_u16(),
                            duration_ms = %latency.as_millis(),
                            "completed"
                        );
                    },
                ),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    // Clean up old agent logs on startup (keep last 20, remove >24h old)
    cleanup_old_agent_logs(std::path::Path::new("/var/log/beam"), 24 * 3600, 20);

    // Print startup banner
    tracing::info!("===========================================");
    tracing::info!(
        "  Beam Remote Desktop Server v{}",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("  {}", startup_banner_line(&bind_addr));
    tracing::info!("===========================================");

    // Bind and serve with TLS
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind to {bind_addr}"))?;

    tracing::info!("Server ready, accepting connections");

    // Background task: reap stale sessions (configurable idle timeout)
    let idle_timeout = state.config.session.idle_timeout;
    if idle_timeout > 0 {
        let reaper_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let stale = reaper_state
                    .session_manager
                    .stale_sessions(idle_timeout)
                    .await;
                for session_id in stale {
                    tracing::info!(%session_id, "Reaping stale session (idle > {idle_timeout}s)");
                    if let Err(e) = reaper_state
                        .session_manager
                        .destroy_session(session_id)
                        .await
                    {
                        tracing::error!(%session_id, "Failed to reap session: {e}");
                    }
                    signaling::remove_channel(&reaper_state.channels, session_id).await;
                    reaper_state.client_metrics.remove(session_id);
                }
            }
        });
    } else {
        tracing::info!("Session idle timeout disabled (idle_timeout = 0)");
    }

    // Set up graceful shutdown
    let shutdown_state = Arc::clone(&state);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // Accept TLS connections and serve with axum
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer_addr) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!("Failed to accept TCP connection: {e}");
                        continue;
                    }
                };

                let acceptor = tls_acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    // TLS handshake timeout (10 seconds)
                    let tls_stream = match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        acceptor.accept(stream),
                    ).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            tracing::debug!(%peer_addr, "TLS handshake failed: {e}");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!(%peer_addr, "TLS handshake timed out");
                            return;
                        }
                    };

                    // Inject peer address so handlers can extract client IP
                    let app_with_peer = app.layer(axum::Extension(peer_addr));

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let hyper_service = hyper_util::service::TowerToHyperService::new(app_with_peer);
                    let builder = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );

                    if let Err(e) = builder.serve_connection_with_upgrades(io, hyper_service).await {
                        tracing::debug!(%peer_addr, "Connection error: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT, initiating graceful shutdown");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, initiating graceful shutdown");
                break;
            }
        }
    }

    // Graceful shutdown: persist sessions so agents survive the restart
    tracing::info!("Persisting sessions for graceful restart...");
    if let Err(e) = shutdown_state.session_manager.persist_sessions().await {
        tracing::error!("Failed to persist sessions, destroying instead: {e}");
        // Fallback: destroy all sessions if persistence fails
        let sessions = shutdown_state.session_manager.list_sessions().await;
        for session in &sessions {
            let _ = shutdown_state
                .session_manager
                .destroy_session(session.id)
                .await;
            signaling::remove_channel(&shutdown_state.channels, session.id).await;
            shutdown_state.client_metrics.remove(session.id);
        }
    }

    tracing::info!("Beam server shut down cleanly (sessions persisted)");

    Ok(())
}

/// Remove old agent logs from `dir`, keeping at most `max_count`
/// and removing any older than `max_age_secs`.
///
/// Only files named `agent-*.log` are considered for cleanup; other files in
/// the directory are left untouched. Errors at any stage (directory missing,
/// metadata unreadable, unlink failure) are swallowed so a hostile filesystem
/// state never blocks server startup.
fn cleanup_old_agent_logs(dir: &std::path::Path, max_age_secs: u64, max_count: usize) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let now = std::time::SystemTime::now();
    let mut logs: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".log"))
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();

    // Sort newest first
    logs.sort_by_key(|b| std::cmp::Reverse(b.1));

    for (i, (path, mtime)) in logs.iter().enumerate() {
        let age = now.duration_since(*mtime).unwrap_or_default().as_secs();
        if i >= max_count || age > max_age_secs {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::cleanup_old_agent_logs;
    use std::time::{Duration, SystemTime};

    fn unique_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-cleanup-test-{}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            label,
        ))
    }

    fn write_log(dir: &std::path::Path, name: &str, age_secs: u64) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"log contents").unwrap();
        // Rewind mtime so the age-filter has something to bite.
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        let ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_mtime(&path, ft).unwrap();
        path
    }

    #[test]
    fn cleanup_creates_dir_when_missing_and_returns_quietly() {
        let dir = unique_dir("creates");
        assert!(!dir.exists());
        // Empty dir; nothing to remove, but the call must not panic and must
        // create the directory so the caller can write logs after startup.
        cleanup_old_agent_logs(&dir, 24 * 3600, 20);
        assert!(dir.exists(), "cleanup should ensure the log dir exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_files_older_than_max_age() {
        let dir = unique_dir("age");
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = write_log(&dir, "agent-001.log", 60);
        let stale = write_log(&dir, "agent-002.log", 7 * 24 * 3600);
        // max_age = 1h
        cleanup_old_agent_logs(&dir, 3600, 100);
        assert!(fresh.exists(), "Fresh log must be retained");
        assert!(!stale.exists(), "Stale log must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_caps_total_count_keeping_newest() {
        let dir = unique_dir("count");
        std::fs::create_dir_all(&dir).unwrap();
        // Five logs aged 60..300s; max_count=2 should keep the two freshest.
        let oldest = write_log(&dir, "agent-old1.log", 300);
        let mid1 = write_log(&dir, "agent-old2.log", 240);
        let mid2 = write_log(&dir, "agent-old3.log", 180);
        let new1 = write_log(&dir, "agent-new1.log", 120);
        let new2 = write_log(&dir, "agent-new2.log", 60);

        cleanup_old_agent_logs(&dir, 24 * 3600, 2);

        assert!(new1.exists(), "Second-newest must survive");
        assert!(new2.exists(), "Newest must survive");
        assert!(!mid1.exists(), "Excess older logs must be removed");
        assert!(!mid2.exists(), "Excess older logs must be removed");
        assert!(!oldest.exists(), "Oldest must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_ignores_non_agent_files() {
        let dir = unique_dir("filter");
        std::fs::create_dir_all(&dir).unwrap();
        // Names that don't match the agent-*.log pattern stay put even
        // when they would otherwise be in the "too old" bucket.
        let unrelated = write_log(&dir, "server.log", 30 * 24 * 3600);
        let other = write_log(&dir, "agent-001.txt", 30 * 24 * 3600);
        let agent_stale = write_log(&dir, "agent-001.log", 30 * 24 * 3600);

        cleanup_old_agent_logs(&dir, 24 * 3600, 100);

        assert!(unrelated.exists(), "Non-agent log must be left alone");
        assert!(other.exists(), "Non-.log extension must be left alone");
        assert!(!agent_stale.exists(), "Matching stale agent log removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod args_tests {
    use super::{ArgsOutcome, parse_args_from};
    use std::path::PathBuf;

    fn run_outcome(args: &[&str]) -> ArgsOutcome {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_arguments() {
        let outcome = run_outcome(&["beam-server"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn config_long_flag_overrides_path() {
        let outcome = run_outcome(&["beam-server", "--config", "/etc/beam/server.toml"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("/etc/beam/server.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn config_short_flag_overrides_path() {
        let outcome = run_outcome(&["beam-server", "-c", "./custom.toml"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./custom.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn port_long_flag_sets_override() {
        let outcome = run_outcome(&["beam-server", "--port", "9443"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: Some(9443),
            }
        );
    }

    #[test]
    fn port_short_flag_sets_override() {
        let outcome = run_outcome(&["beam-server", "-p", "443"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: Some(443),
            }
        );
    }

    #[test]
    fn port_non_numeric_is_silently_dropped() {
        // Bad port input → keep port_override at None rather than crashing.
        let outcome = run_outcome(&["beam-server", "--port", "not-a-number"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn port_out_of_u16_range_is_silently_dropped() {
        let outcome = run_outcome(&["beam-server", "-p", "70000"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn config_and_port_combined() {
        let outcome = run_outcome(&["beam-server", "--config", "/tmp/x.toml", "--port", "8443"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("/tmp/x.toml"),
                port_override: Some(8443),
            }
        );
    }

    #[test]
    fn config_and_port_short_combined() {
        let outcome = run_outcome(&["beam-server", "-c", "/tmp/y.toml", "-p", "12345"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("/tmp/y.toml"),
                port_override: Some(12345),
            }
        );
    }

    #[test]
    fn version_long_flag_returns_print_version() {
        assert_eq!(
            run_outcome(&["beam-server", "--version"]),
            ArgsOutcome::PrintVersion
        );
    }

    #[test]
    fn version_short_flag_returns_print_version() {
        assert_eq!(
            run_outcome(&["beam-server", "-V"]),
            ArgsOutcome::PrintVersion
        );
    }

    #[test]
    fn help_long_flag_returns_print_help() {
        assert_eq!(
            run_outcome(&["beam-server", "--help"]),
            ArgsOutcome::PrintHelp
        );
    }

    #[test]
    fn help_short_flag_returns_print_help() {
        assert_eq!(run_outcome(&["beam-server", "-h"]), ArgsOutcome::PrintHelp);
    }

    #[test]
    fn unknown_flag_is_ignored() {
        // Unrecognized arguments don't crash the parser; defaults stand.
        let outcome = run_outcome(&["beam-server", "--what-is-this", "extra"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn trailing_config_flag_without_value_keeps_default() {
        // "--config" at the very end has no following argv slot.
        let outcome = run_outcome(&["beam-server", "--config"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn trailing_port_flag_without_value_keeps_default() {
        let outcome = run_outcome(&["beam-server", "-p"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn version_short_circuits_remaining_args() {
        // Once we see --version we return immediately, so a later --port is ignored.
        assert_eq!(
            run_outcome(&["beam-server", "--version", "--port", "9999"]),
            ArgsOutcome::PrintVersion,
        );
    }

    #[test]
    fn help_short_circuits_remaining_args() {
        assert_eq!(
            run_outcome(&["beam-server", "-h", "--config", "/etc/x"]),
            ArgsOutcome::PrintHelp,
        );
    }

    #[test]
    fn duplicate_port_uses_last_value() {
        // The simple while-loop overwrites earlier overrides with later ones.
        let outcome = run_outcome(&["beam-server", "-p", "1111", "-p", "2222"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: Some(2222),
            }
        );
    }

    #[test]
    fn duplicate_config_uses_last_value() {
        let outcome = run_outcome(&["beam-server", "-c", "a.toml", "-c", "b.toml"]);
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("b.toml"),
                port_override: None,
            }
        );
    }

    #[test]
    fn empty_argv_returns_defaults() {
        // Defensive: even an empty argv vector (no argv[0]) should not panic.
        let outcome = parse_args_from(Vec::<String>::new());
        assert_eq!(
            outcome,
            ArgsOutcome::Run {
                config_path: PathBuf::from("./config/beam.toml"),
                port_override: None,
            }
        );
    }
}

#[cfg(test)]
mod bind_addr_tests {
    use super::parse_bind_addr;

    #[test]
    fn ipv4_bind_address_parses() {
        let addr = parse_bind_addr("127.0.0.1", 8443).unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:8443");
    }

    #[test]
    fn ipv4_zero_bind_address_parses() {
        let addr = parse_bind_addr("0.0.0.0", 8444).unwrap();
        assert_eq!(addr.to_string(), "0.0.0.0:8444");
    }

    #[test]
    fn ipv6_localhost_parses() {
        // Bracketed IPv6 literal.
        let addr = parse_bind_addr("[::1]", 8444).unwrap();
        assert!(addr.to_string().contains("8444"));
        assert!(addr.is_ipv6());
    }

    #[test]
    fn ipv6_any_parses() {
        let addr = parse_bind_addr("[::]", 9000).unwrap();
        assert!(addr.is_ipv6());
        assert_eq!(addr.port(), 9000);
    }

    #[test]
    fn invalid_bind_address_returns_error() {
        let result = parse_bind_addr("not-an-ip-address", 8443);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_bind_address_error_mentions_invalid() {
        let err = parse_bind_addr("not-an-ip-address", 8443).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("invalid"),
            "Error should mention 'Invalid': {msg}"
        );
    }

    #[test]
    fn port_zero_is_valid() {
        // Port 0 means "let the OS pick" — must be parseable even if it's
        // unusual for a production beam config.
        let addr = parse_bind_addr("127.0.0.1", 0).unwrap();
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn port_max_is_valid() {
        let addr = parse_bind_addr("127.0.0.1", u16::MAX).unwrap();
        assert_eq!(addr.port(), u16::MAX);
    }

    #[test]
    fn hostname_does_not_parse() {
        // SocketAddr::parse does not do DNS resolution — hostnames must fail.
        let result = parse_bind_addr("localhost", 8443);
        assert!(result.is_err(), "hostname should not resolve");
    }
}

#[cfg(test)]
mod jwt_secret_tests {
    use super::{load_persisted_jwt_secret, persist_jwt_secret};
    use std::os::unix::fs::PermissionsExt;

    fn unique_secret_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-jwt-secret-{}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            label,
        ))
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let path = unique_secret_path("missing");
        assert!(load_persisted_jwt_secret(&path).is_none());
    }

    #[test]
    fn load_returns_none_for_empty_file() {
        let path = unique_secret_path("empty");
        std::fs::write(&path, "").unwrap();
        assert!(load_persisted_jwt_secret(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_returns_none_for_whitespace_only() {
        let path = unique_secret_path("whitespace");
        std::fs::write(&path, "   \n\t  \n").unwrap();
        assert!(load_persisted_jwt_secret(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_trims_secret() {
        let path = unique_secret_path("trim");
        std::fs::write(&path, "  abcdef0123456789  \n").unwrap();
        let secret = load_persisted_jwt_secret(&path).expect("should load");
        assert_eq!(secret, "abcdef0123456789");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_returns_full_content_when_no_whitespace() {
        let path = unique_secret_path("clean");
        let secret = "1234567890abcdef".repeat(4);
        std::fs::write(&path, &secret).unwrap();
        let loaded = load_persisted_jwt_secret(&path).expect("should load");
        assert_eq!(loaded, secret);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_writes_secret_with_0600_perms() {
        // tmp dir requires creating an intermediate subdirectory so we can
        // exercise the `create_dir_all` arm.
        let dir = std::env::temp_dir().join(format!(
            "beam-jwt-persist-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = dir.join("jwt_secret");

        assert!(!dir.exists(), "Subdir must not pre-exist");
        let secret = "test-secret-1234567890";
        persist_jwt_secret(&path, secret).expect("persist should succeed");

        assert!(dir.exists(), "create_dir_all should have run");
        assert!(path.exists(), "secret file should be written");

        let loaded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(loaded, secret);

        let perms = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(perms, 0o600, "secret file must be 0o600");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_truncates_existing_file() {
        // Pre-write a longer secret, then re-persist a shorter one — the
        // file must end up containing only the new secret.
        let path = unique_secret_path("truncate");
        std::fs::write(&path, "this-was-the-old-much-longer-secret").unwrap();

        persist_jwt_secret(&path, "new-shorter").unwrap();
        let loaded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(loaded, "new-shorter");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_fails_on_unwritable_parent() {
        // Writing under /proc returns an OS error — verify the function
        // surfaces it rather than panicking.
        let path = std::path::PathBuf::from("/proc/nonexistent-beam-jwt-secret");
        let result = persist_jwt_secret(&path, "x");
        assert!(result.is_err(), "Should fail when path is unwritable");
    }

    #[test]
    fn load_after_persist_roundtrip() {
        let path = unique_secret_path("roundtrip");
        let secret = "deadbeef0123456789abcdef";
        persist_jwt_secret(&path, secret).unwrap();
        let loaded = load_persisted_jwt_secret(&path).unwrap();
        assert_eq!(loaded, secret);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod resolve_jwt_secret_tests {
    use super::{JwtSecretSource, resolve_jwt_secret};

    fn unique_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-jwt-resolve-{}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            label,
        ))
    }

    #[test]
    fn configured_secret_wins_over_disk() {
        // Even if a path exists, an explicit config override skips disk.
        let path = unique_path("configured-wins");
        std::fs::write(&path, "from-disk-12345").unwrap();
        let (secret, source) =
            resolve_jwt_secret(Some("from-config-67890".to_string()), &path, || {
                panic!("generator should not run when config is set")
            });
        assert_eq!(secret, "from-config-67890");
        assert_eq!(source, JwtSecretSource::LoadedFromDisk);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_disk_triggers_generator_and_persists() {
        let path = unique_path("generate-persist");
        assert!(!path.exists());
        let (secret, source) =
            resolve_jwt_secret(None, &path, || "generated-secret-abcdef".to_string());
        assert_eq!(secret, "generated-secret-abcdef");
        assert_eq!(source, JwtSecretSource::GeneratedAndPersisted);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert_eq!(stored, "generated-secret-abcdef");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loaded_from_disk_when_present() {
        let path = unique_path("load");
        std::fs::write(&path, "preexisting-secret-xyz123").unwrap();
        let (secret, source) = resolve_jwt_secret(None, &path, || {
            panic!("generator should not run when secret is on disk")
        });
        assert_eq!(secret, "preexisting-secret-xyz123");
        assert_eq!(source, JwtSecretSource::LoadedFromDisk);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ephemeral_when_persist_fails() {
        // /proc/foo is unwritable — persist_jwt_secret returns Err.
        let path = std::path::PathBuf::from("/proc/nonexistent-beam-jwt-resolve");
        let (secret, source) = resolve_jwt_secret(None, &path, || "ephemeral-secret".to_string());
        assert_eq!(secret, "ephemeral-secret");
        assert!(matches!(source, JwtSecretSource::GeneratedEphemeral(_)));
    }

    #[test]
    fn empty_disk_file_falls_back_to_generator() {
        // An empty file on disk reads as None per load_persisted_jwt_secret,
        // so the generator runs and the result is persisted (overwriting
        // the empty file).
        let path = unique_path("empty");
        std::fs::write(&path, "").unwrap();
        let (secret, source) = resolve_jwt_secret(None, &path, || "fresh-secret".to_string());
        assert_eq!(secret, "fresh-secret");
        assert_eq!(source, JwtSecretSource::GeneratedAndPersisted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn whitespace_disk_file_falls_back_to_generator() {
        let path = unique_path("whitespace");
        std::fs::write(&path, "   \n\t  ").unwrap();
        let (secret, source) = resolve_jwt_secret(None, &path, || "real-secret".to_string());
        assert_eq!(secret, "real-secret");
        assert_eq!(source, JwtSecretSource::GeneratedAndPersisted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn configured_empty_string_still_overrides() {
        // An explicit `Some("")` is intentional — it skips disk + generator.
        // (Production validate() catches this earlier, but the function is
        // deterministic about Option semantics.)
        let path = unique_path("empty-config");
        std::fs::write(&path, "real-disk-secret").unwrap();
        let (secret, source) = resolve_jwt_secret(Some(String::new()), &path, || {
            panic!("generator should not run")
        });
        assert_eq!(secret, String::new());
        assert_eq!(source, JwtSecretSource::LoadedFromDisk);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod startup_helper_tests {
    use super::{
        count_validation_severities, pam_missing_warning, startup_banner_line, web_root_is_usable,
        web_root_missing_warning,
    };
    use std::net::SocketAddr;

    // --- count_validation_severities ---

    #[test]
    fn validation_counts_zero_for_empty() {
        let (e, w) = count_validation_severities(&[]);
        assert_eq!(e, 0);
        assert_eq!(w, 0);
    }

    #[test]
    fn validation_counts_only_errors() {
        let issues = vec!["ERROR: a".to_string(), "ERROR: b".to_string()];
        let (e, w) = count_validation_severities(&issues);
        assert_eq!(e, 2);
        assert_eq!(w, 0);
    }

    #[test]
    fn validation_counts_only_warnings() {
        let issues = vec!["WARN: a".to_string(), "WARN: b".to_string()];
        let (e, w) = count_validation_severities(&issues);
        assert_eq!(e, 0);
        assert_eq!(w, 2);
    }

    #[test]
    fn validation_counts_mixed() {
        let issues = vec![
            "ERROR: a".to_string(),
            "WARN: b".to_string(),
            "ERROR: c".to_string(),
            "anything else".to_string(),
        ];
        let (e, w) = count_validation_severities(&issues);
        assert_eq!(e, 2);
        assert_eq!(w, 2);
    }

    #[test]
    fn validation_counts_strict_prefix() {
        // Only "ERROR:" at the start counts; substring matches don't.
        let issues = vec!["WARN: includes ERROR: text".to_string()];
        let (e, w) = count_validation_severities(&issues);
        assert_eq!(e, 0);
        assert_eq!(w, 1);
    }

    // --- pam_missing_warning ---

    #[test]
    fn pam_warning_mentions_path() {
        let msg = pam_missing_warning();
        assert!(msg.contains("/etc/pam.d/beam"));
        assert!(msg.contains("agent sessions"));
    }

    #[test]
    fn pam_warning_suggests_remediation() {
        let msg = pam_missing_warning();
        assert!(msg.contains("Install") || msg.contains("packaging"));
    }

    // --- web_root_missing_warning ---

    #[test]
    fn web_root_warning_mentions_configured_path() {
        let msg = web_root_missing_warning("/srv/beam/web");
        assert!(msg.contains("/srv/beam/web"));
        assert!(msg.contains("UI will not load"));
    }

    #[test]
    fn web_root_warning_suggests_build_command() {
        let msg = web_root_missing_warning("/tmp/x");
        assert!(msg.contains("make build-web") || msg.contains("server.web_root"));
    }

    #[test]
    fn web_root_warning_handles_empty_path() {
        let msg = web_root_missing_warning("");
        assert!(msg.contains("''"));
    }

    // --- web_root_is_usable ---

    #[test]
    fn web_root_usable_false_for_empty() {
        assert!(!web_root_is_usable(""));
    }

    #[test]
    fn web_root_usable_false_for_nonexistent() {
        assert!(!web_root_is_usable(
            "/tmp/beam-nonexistent-web-root-test-q9r2"
        ));
    }

    #[test]
    fn web_root_usable_true_for_existing_dir() {
        // /tmp exists on every supported system.
        assert!(web_root_is_usable("/tmp"));
    }

    #[test]
    fn web_root_usable_false_for_file() {
        // A regular file is not a usable web root.
        let path = std::env::temp_dir().join(format!(
            "beam-web-root-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
        ));
        std::fs::write(&path, "not a dir").unwrap();
        assert!(!web_root_is_usable(path.to_str().unwrap()));
        let _ = std::fs::remove_file(&path);
    }

    // --- startup_banner_line ---

    #[test]
    fn banner_includes_https_scheme() {
        let addr: SocketAddr = "127.0.0.1:8443".parse().unwrap();
        let line = startup_banner_line(&addr);
        assert!(line.contains("https://"));
        assert!(line.contains("127.0.0.1:8443"));
    }

    #[test]
    fn banner_handles_ipv6() {
        let addr: SocketAddr = "[::1]:8443".parse().unwrap();
        let line = startup_banner_line(&addr);
        assert!(line.contains("[::1]"));
        assert!(line.contains("8443"));
    }

    #[test]
    fn banner_format_starts_with_listening() {
        let addr: SocketAddr = "0.0.0.0:443".parse().unwrap();
        let line = startup_banner_line(&addr);
        assert!(line.starts_with("Listening on"));
    }
}

#[cfg(test)]
mod validation_tests {
    use super::{config_validation_summary, has_validation_errors};

    #[test]
    fn no_errors_when_all_warnings() {
        // Only "WARN:" prefix → not an error.
        let issues: Vec<String> = vec!["WARN: tls_cert missing".into()];
        assert!(!has_validation_errors(&issues));
    }

    #[test]
    fn no_errors_for_empty_issues_list() {
        assert!(!has_validation_errors(&[]));
    }

    #[test]
    fn has_errors_when_any_error_prefix() {
        // A single "ERROR:" line flips the classifier.
        let issues: Vec<String> = vec![
            "WARN: tls_cert missing".into(),
            "ERROR: invalid port".into(),
        ];
        assert!(has_validation_errors(&issues));
    }

    #[test]
    fn has_errors_only_strict_prefix() {
        // "error:" lowercase is NOT an ERROR (config.validate() uses ERROR:).
        let issues: Vec<String> = vec!["error: lowercase".into()];
        assert!(!has_validation_errors(&issues));
    }

    #[test]
    fn has_errors_requires_prefix() {
        // Substring "ERROR:" in the middle doesn't count.
        let issues: Vec<String> = vec!["WARN: contains ERROR: word".into()];
        assert!(!has_validation_errors(&issues));
    }

    #[test]
    fn summary_includes_issue_count() {
        let issues: Vec<String> = vec!["ERROR: a".into(), "ERROR: b".into(), "WARN: c".into()];
        let summary = config_validation_summary(&issues);
        assert!(summary.contains("3 issue"));
    }

    #[test]
    fn summary_mentions_restart() {
        let issues: Vec<String> = vec!["ERROR: x".into()];
        let summary = config_validation_summary(&issues);
        assert!(
            summary.to_lowercase().contains("restart") || summary.contains("Fix"),
            "Summary should guide the operator to fix + restart: {summary}"
        );
    }

    #[test]
    fn summary_handles_empty_issues() {
        // Edge case: caller invokes this with no issues. Should still format
        // cleanly (production never hits this branch but defensive testing).
        let summary = config_validation_summary(&[]);
        assert!(summary.contains("0 issue"));
    }

    #[test]
    fn summary_with_single_issue_uses_singular_count() {
        let issues = vec!["ERROR: only one".to_string()];
        let summary = config_validation_summary(&issues);
        assert!(summary.contains("1 issue"));
    }
}
