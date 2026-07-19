//! Backend Sentry SDK wiring: client init, panic/tracing integration, and a
//! `before_send` scrub hook.
//!
//! Kept separate from `main.rs` so the scrub logic (the security-sensitive
//! part) is independently unit-testable and small.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// How long to block on flushing pending events before giving up. Applied at
/// every explicit process-exit point (see `flush_and_exit` /
/// `flush_on_shutdown`) since `std::process::exit` skips destructors and
/// would otherwise silently drop any event captured between init and exit.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Bearer tokens: `Bearer <token>` (RFC 6750 token68 charset).
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Bearer [A-Za-z0-9._~+/-]+=*").expect("valid regex"));

/// JWT-shaped substrings: three base64url segments separated by dots, the
/// first of which starts with the near-universal `eyJ` JSON-header prefix.
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}")
        .expect("valid regex")
});

const REDACTED: &str = "[redacted]";

/// Redact bearer-token/JWT-shaped substrings from a string. Used on event
/// messages and breadcrumb messages before they leave the process.
fn scrub_str(input: &str) -> String {
    let step1 = BEARER_RE.replace_all(input, REDACTED);
    JWT_RE.replace_all(&step1, REDACTED).into_owned()
}

/// Names that, if they ever show up as a structured `extra`/tag key, get
/// their value redacted outright regardless of shape — defense in depth for
/// any future code path that attaches request/session context as extras.
const SENSITIVE_KEY_NAMES: [&str; 5] = ["password", "secret", "token", "jwt", "authorization"];

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_NAMES.iter().any(|s| lower.contains(s))
}

/// `before_send` hook: redacts bearer-token/JWT-shaped substrings (and any
/// extra/tag whose key name looks like a credential) from an event before
/// it's transmitted to Sentry.
///
/// This is defense-in-depth, not the primary control — the real control is
/// that `auth.rs` never logs raw credentials (see `authenticate_pam` /
/// `generate_jwt`, which log only usernames and error summaries). This hook
/// exists because `tracing::error!` now forwards to Sentry via
/// `sentry_tracing::layer()`, so any *future* log line that accidentally
/// interpolates a credential no longer only leaks to local logs — it would
/// also leave the process. Scrubbing here is the safety net.
pub fn before_send(
    mut event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    if let Some(message) = event.message.take() {
        event.message = Some(scrub_str(&message));
    }

    let extra_keys: Vec<String> = event
        .extra
        .iter()
        .filter(|(k, _)| is_sensitive_key(k))
        .map(|(k, _)| k.clone())
        .collect();
    for key in extra_keys {
        event
            .extra
            .insert(key, serde_json::Value::String(REDACTED.to_string()));
    }

    let tag_keys: Vec<String> = event
        .tags
        .iter()
        .filter(|(k, _)| is_sensitive_key(k))
        .map(|(k, _)| k.clone())
        .collect();
    for key in tag_keys {
        event.tags.insert(key, REDACTED.to_string());
    }

    for crumb in &mut event.breadcrumbs.values {
        if let Some(message) = crumb.message.take() {
            crumb.message = Some(scrub_str(&message));
        }
    }

    Some(event)
}

/// Initialize the backend Sentry client if `dsn` is present and non-empty.
///
/// Returns the [`sentry::ClientInitGuard`] when a client was actually
/// created; the caller MUST hold this for the lifetime of `main()` (dropping
/// it flushes the pending-event queue). Returns `None` when no DSN is
/// configured, in which case Sentry integrations are inert no-ops.
pub fn init(dsn: Option<&str>, environment: &str) -> Option<sentry::ClientInitGuard> {
    let dsn = dsn.filter(|d| !d.trim().is_empty())?;

    let guard = sentry::init((
        dsn,
        sentry::ClientOptions {
            environment: Some(environment.to_string().into()),
            before_send: Some(std::sync::Arc::new(before_send)),
            // `panic` (Cargo feature) + default_integrations covers the
            // panic hook; explicit here for clarity that we rely on it.
            default_integrations: true,
            ..Default::default()
        },
    ));

    if guard.is_enabled() {
        Some(guard)
    } else {
        None
    }
}

/// Flush any pending Sentry events with a bounded timeout. Call this
/// immediately before every `std::process::exit()` that occurs after
/// `init()` — `process::exit` skips destructors, so the `ClientInitGuard`
/// returned by `init()` would otherwise never flush.
pub fn flush() {
    if let Some(client) = sentry::Hub::current().client() {
        client.close(Some(FLUSH_TIMEOUT));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_bearer_token() {
        let input = "request failed: Authorization: Bearer abc123.def-456_ghi";
        let out = scrub_str(input);
        assert!(!out.contains("abc123"));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn scrubs_jwt_shaped_substring() {
        // Built from separate non-suspicious segments (rather than one JWT-
        // shaped literal) so this synthetic fixture doesn't itself trip the
        // repo's gitleaks JWT rule.
        let header = ["eyJhbGciOiJIUzI1", "NiJ9"].concat();
        let payload = ["eyJzdWIiOiJ1c2Vy", "In0"].concat();
        let signature = ["dGhpc2lzc2ln", "bmF0dXJl"].concat();
        let jwt = format!("{header}.{payload}.{signature}");
        let input = format!("token was {jwt}");
        let out = scrub_str(&input);
        assert!(!out.contains(&jwt));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn leaves_unrelated_text_untouched() {
        let input = "Failed to destroy session: connection reset";
        assert_eq!(scrub_str(input), input);
    }

    #[test]
    fn detects_sensitive_key_names_case_insensitively() {
        assert!(is_sensitive_key("password"));
        assert!(is_sensitive_key("Password"));
        assert!(is_sensitive_key("user_secret"));
        assert!(is_sensitive_key("API_TOKEN"));
        assert!(is_sensitive_key("Authorization"));
        assert!(!is_sensitive_key("username"));
        assert!(!is_sensitive_key("session_id"));
    }

    #[test]
    fn init_returns_none_for_absent_dsn() {
        assert!(init(None, "test").is_none());
    }

    #[test]
    fn init_returns_none_for_empty_dsn() {
        assert!(init(Some(""), "test").is_none());
        assert!(init(Some("   "), "test").is_none());
    }
}
