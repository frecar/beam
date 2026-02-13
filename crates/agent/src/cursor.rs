use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes;
use x11rb::rust_connection::RustConnection;

/// Spawn a thread that monitors X11 cursor shape changes via XFixes
/// and sends CSS cursor names over the returned channel.
///
/// Also hides the X11 cursor on the display so it doesn't appear
/// in the screen capture — the browser renders its own CSS cursor instead.
pub fn spawn_cursor_monitor(display: &str) -> Option<mpsc::Receiver<String>> {
    let display = display.to_string();
    let (tx, rx) = mpsc::channel::<String>(8);

    std::thread::Builder::new()
        .name("cursor-monitor".into())
        .spawn(move || {
            if let Err(e) = cursor_monitor_loop(&display, tx) {
                warn!("Cursor monitor exited: {e:#}");
            }
        })
        .ok()?;

    Some(rx)
}

fn cursor_monitor_loop(display: &str, tx: mpsc::Sender<String>) -> anyhow::Result<()> {
    let (conn, screen_num) =
        RustConnection::connect(Some(display)).map_err(|e| anyhow::anyhow!("X11 connect: {e}"))?;

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // Check XFixes version (need >= 4 for cursor name support)
    let version = xfixes::query_version(&conn, 4, 0)?.reply()?;
    if version.major_version < 4 {
        anyhow::bail!(
            "XFixes version {}.{} too old (need >= 4.0)",
            version.major_version,
            version.minor_version
        );
    }

    // Hide cursor on this display so it doesn't appear in screen capture.
    // XFixes HideCursor makes the cursor invisible but cursor change events
    // still fire, which is exactly what we want.
    xfixes::hide_cursor(&conn, root)?;
    conn.flush()?;
    info!("Cursor hidden via XFixes HideCursor");

    // Subscribe to cursor change events
    xfixes::select_cursor_input(&conn, root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    conn.flush()?;

    let mut last_css = String::new();
    let mut last_serial: u32 = 0;

    // Build name→CSS mapping table
    let map = build_cursor_map();

    // Send initial cursor state
    if let Ok(reply) = xfixes::get_cursor_image_and_name(&conn)?.reply() {
        let name = String::from_utf8_lossy(&reply.name).to_string();
        let css = map
            .get(name.as_str())
            .copied()
            .unwrap_or("default")
            .to_string();
        last_css = css.clone();
        last_serial = reply.cursor_serial;
        let _ = tx.blocking_send(css);
    }

    loop {
        let event = conn.wait_for_event()?;

        if let Event::XfixesCursorNotify(notify) = event {
            // Dedup by serial — same cursor shape, skip
            if notify.cursor_serial == last_serial {
                continue;
            }
            last_serial = notify.cursor_serial;

            // Get cursor name
            let css = match xfixes::get_cursor_image_and_name(&conn)?.reply() {
                Ok(reply) => {
                    let name = String::from_utf8_lossy(&reply.name).to_string();
                    debug!(cursor_name = %name, "Cursor changed");
                    map.get(name.as_str())
                        .copied()
                        .unwrap_or("default")
                        .to_string()
                }
                Err(_) => "default".to_string(),
            };

            // Only send if changed
            if css != last_css {
                last_css = css.clone();
                if tx.blocking_send(css).is_err() {
                    break; // receiver dropped, agent shutting down
                }
            }
        }
    }

    Ok(())
}

fn build_cursor_map() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();

    // Default / arrow
    for name in ["left_ptr", "default", "arrow", "top_left_arrow"] {
        m.insert(name, "default");
    }

    // Text cursor
    for name in ["xterm", "text", "ibeam"] {
        m.insert(name, "text");
    }

    // Pointer / hand
    for name in ["hand2", "hand1", "pointer", "pointing_hand", "hand"] {
        m.insert(name, "pointer");
    }

    // Wait
    for name in ["watch", "wait"] {
        m.insert(name, "wait");
    }

    // Progress
    for name in ["left_ptr_watch", "progress", "half-busy"] {
        m.insert(name, "progress");
    }

    // Crosshair
    for name in ["crosshair", "cross", "tcross", "cross_reverse"] {
        m.insert(name, "crosshair");
    }

    // Resize handles
    for name in [
        "sb_h_double_arrow",
        "ew-resize",
        "col-resize",
        "h_double_arrow",
        "size_hor",
    ] {
        m.insert(name, "ew-resize");
    }
    for name in [
        "sb_v_double_arrow",
        "ns-resize",
        "row-resize",
        "v_double_arrow",
        "size_ver",
    ] {
        m.insert(name, "ns-resize");
    }
    for name in ["top_left_corner", "nw-resize", "size_fdiag", "nwse-resize"] {
        m.insert(name, "nw-resize");
    }
    for name in ["top_right_corner", "ne-resize", "size_bdiag", "nesw-resize"] {
        m.insert(name, "ne-resize");
    }
    for name in ["bottom_left_corner", "sw-resize"] {
        m.insert(name, "sw-resize");
    }
    for name in ["bottom_right_corner", "se-resize"] {
        m.insert(name, "se-resize");
    }
    for name in ["top_side", "n-resize"] {
        m.insert(name, "n-resize");
    }
    for name in ["bottom_side", "s-resize"] {
        m.insert(name, "s-resize");
    }
    for name in ["left_side", "w-resize"] {
        m.insert(name, "w-resize");
    }
    for name in ["right_side", "e-resize"] {
        m.insert(name, "e-resize");
    }

    // Move / drag
    for name in ["fleur", "move", "all-scroll", "size_all"] {
        m.insert(name, "move");
    }

    // Not allowed
    for name in [
        "not-allowed",
        "crossed_circle",
        "X_cursor",
        "forbidden",
        "no-drop",
    ] {
        m.insert(name, "not-allowed");
    }

    // Help
    for name in ["question_arrow", "help", "whats_this"] {
        m.insert(name, "help");
    }

    // Grab
    for name in ["grab", "openhand"] {
        m.insert(name, "grab");
    }
    for name in ["closedhand", "grabbing", "dnd-move", "dnd-copy", "dnd-link"] {
        m.insert(name, "grabbing");
    }

    // Context menu
    m.insert("context-menu", "context-menu");

    // Cell / table select
    m.insert("plus", "cell");
    m.insert("cell", "cell");

    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_map_covers_common_names() {
        let map = build_cursor_map();
        assert_eq!(map.get("left_ptr"), Some(&"default"));
        assert_eq!(map.get("xterm"), Some(&"text"));
        assert_eq!(map.get("hand2"), Some(&"pointer"));
        assert_eq!(map.get("watch"), Some(&"wait"));
        assert_eq!(map.get("sb_h_double_arrow"), Some(&"ew-resize"));
        assert_eq!(map.get("sb_v_double_arrow"), Some(&"ns-resize"));
        assert_eq!(map.get("fleur"), Some(&"move"));
        assert_eq!(map.get("crossed_circle"), Some(&"not-allowed"));
        assert_eq!(map.get("question_arrow"), Some(&"help"));
    }

    #[test]
    fn cursor_map_unknown_returns_none() {
        let map = build_cursor_map();
        assert_eq!(map.get("some_custom_cursor_xyz"), None);
    }
}
