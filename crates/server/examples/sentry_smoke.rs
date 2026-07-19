//! Manual verification harness for the backend Sentry wiring.
//!
//! This is a `cargo run --example` target only — it is NOT compiled into the
//! shipped `beam-server` binary, NOT part of the `.deb` package, and does not
//! add any route or attack surface to the running server (a debug HTTP route
//! for this was considered and rejected: it would be unnecessary production
//! attack surface for something that only needs to run once per deployment).
//!
//! ## How to run
//!
//! ```sh
//! BEAM_SENTRY_BACKEND_DSN='https://<key>@<host>/<project>' \
//!     cargo run --example sentry_smoke -p beam-server
//! ```
//!
//! Never hardcode a real DSN here or pass one on a shared shell history —
//! this is a public repository and a leaked DSN is a permanent credential
//! leak even for a "just testing" run. The DSN only ever comes from the
//! `BEAM_SENTRY_BACKEND_DSN` environment variable.
//!
//! ## How to verify
//!
//! The event is tagged `smoke_test=true` and its message is distinctive
//! ("beam backend Sentry smoke test"). After running, check the target
//! Sentry project's issue stream for an event matching that tag/message,
//! confirm it arrived, then resolve/delete it so it doesn't linger as
//! open-issue noise.

use std::time::Duration;

fn main() {
    let dsn = std::env::var("BEAM_SENTRY_BACKEND_DSN").unwrap_or_default();
    if dsn.trim().is_empty() {
        eprintln!(
            "BEAM_SENTRY_BACKEND_DSN is not set — refusing to run. \
             Set it to the target project's DSN and re-run."
        );
        std::process::exit(1);
    }

    let guard = sentry::init((
        dsn.as_str(),
        sentry::ClientOptions {
            environment: Some("smoke-test".into()),
            ..Default::default()
        },
    ));

    if !guard.is_enabled() {
        eprintln!("Sentry client did not initialize (DSN rejected?) — nothing was sent.");
        std::process::exit(1);
    }

    sentry::configure_scope(|scope| {
        scope.set_tag("smoke_test", "true");
    });

    let event_id = sentry::capture_message("beam backend Sentry smoke test", sentry::Level::Info);
    println!("Captured event {event_id} — flushing...");

    // Explicit flush before exit, same lesson as production main.rs: an
    // example binary that exits without flushing proves nothing, since the
    // pending-event queue is background-flushed on client drop and a plain
    // process exit here would race that background flush.
    let flushed = guard.flush(Some(Duration::from_secs(5)));
    if flushed {
        println!(
            "Flushed successfully. Check the Sentry project's issue stream for event {event_id} (tag smoke_test=true)."
        );
    } else {
        eprintln!("Flush timed out — event may not have been delivered.");
        std::process::exit(1);
    }
}
