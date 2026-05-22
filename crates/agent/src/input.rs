use anyhow::Context;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::info;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

/// Input injector using X11 XTEST extension.
///
/// Injects keyboard, mouse, and scroll events directly into the X server
/// via XTestFakeInput. This bypasses udev/uinput entirely — no kernel
/// device creation needed, works regardless of AutoAddDevices setting.
pub struct InputInjector {
    conn: RustConnection,
    root: xproto::Window,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    /// Accumulated fractional scroll for smooth trackpad support
    scroll_accum_x: f64,
    scroll_accum_y: f64,
}

impl InputInjector {
    pub fn new(
        x_display: &str,
        width: Arc<AtomicU32>,
        height: Arc<AtomicU32>,
    ) -> anyhow::Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(Some(x_display)).context("Failed to connect to X display")?;
        let root = conn.setup().roots[screen_num].root;

        // Verify XTEST extension is available
        let _ = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .context("Failed to query XTEST extension")?
            .ok_or_else(|| anyhow::anyhow!("XTEST extension not available"))?;

        info!(display = x_display, "Input injector initialized via XTEST");
        Ok(Self {
            conn,
            root,
            width,
            height,
            scroll_accum_x: 0.0,
            scroll_accum_y: 0.0,
        })
    }

    /// Inject a keyboard event. `code` is a Linux evdev keycode.
    /// X11 keycode = evdev keycode + 8.
    pub fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        let x_keycode = (code + 8) as u8;
        let event_type = if pressed {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        xtest::fake_input(&self.conn, event_type, x_keycode, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Inject absolute mouse movement from normalized [0.0, 1.0] coordinates.
    pub fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        let w = self.width.load(Ordering::Relaxed);
        let h = self.height.load(Ordering::Relaxed);
        let px = (x.clamp(0.0, 1.0) * w as f64) as i16;
        let py = (y.clamp(0.0, 1.0) * h as f64) as i16;
        // detail=0 for absolute motion, root=target window
        xtest::fake_input(
            &self.conn,
            xproto::MOTION_NOTIFY_EVENT,
            0, // false = absolute
            0,
            self.root,
            px,
            py,
            0,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Inject relative mouse movement (pointer lock mode).
    pub fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let dx_i = dx.round() as i16;
        let dy_i = dy.round() as i16;
        if dx_i == 0 && dy_i == 0 {
            return Ok(());
        }
        // detail=1 for relative motion
        xtest::fake_input(
            &self.conn,
            xproto::MOTION_NOTIFY_EVENT,
            1, // true = relative
            0,
            x11rb::NONE, // no root for relative
            dx_i,
            dy_i,
            0,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Map browser button index to X11 button number.
    /// Browser: 0=left, 1=middle, 2=right → X11: 1=left, 2=middle, 3=right
    fn map_button(button: u8) -> anyhow::Result<u8> {
        match button {
            0 => Ok(1), // left
            1 => Ok(2), // middle
            2 => Ok(3), // right
            _ => anyhow::bail!("Unknown mouse button: {button}"),
        }
    }

    pub fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        let x_button = Self::map_button(button)?;
        let event_type = if pressed {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        xtest::fake_input(&self.conn, event_type, x_button, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Accumulate fractional scroll and return discrete notch count.
    fn accumulate_scroll(accum: &mut f64, pixels_per_notch: f64) -> i32 {
        *accum += pixels_per_notch;
        let discrete = *accum as i32;
        if discrete != 0 {
            *accum -= discrete as f64;
        }
        discrete
    }

    /// Inject scroll events.
    /// X11 scroll uses button 4/5 (vertical) and 6/7 (horizontal).
    /// Each scroll notch is a press+release of the corresponding button.
    pub fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        // Vertical scroll: button 4 = up, button 5 = down
        if dy.abs() > 0.001 {
            let discrete_y = Self::accumulate_scroll(&mut self.scroll_accum_y, -dy / 30.0);
            let (button, count) = if discrete_y > 0 {
                (4u8, discrete_y as u32) // scroll up
            } else if discrete_y < 0 {
                (5u8, (-discrete_y) as u32) // scroll down
            } else {
                (0, 0)
            };
            for _ in 0..count {
                xtest::fake_input(
                    &self.conn,
                    xproto::BUTTON_PRESS_EVENT,
                    button,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                )?;
                xtest::fake_input(
                    &self.conn,
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                )?;
            }
        }

        // Horizontal scroll: button 6 = left, button 7 = right
        if dx.abs() > 0.001 {
            let discrete_x = Self::accumulate_scroll(&mut self.scroll_accum_x, dx / 30.0);
            let (button, count) = if discrete_x > 0 {
                (7u8, discrete_x as u32) // scroll right
            } else if discrete_x < 0 {
                (6u8, (-discrete_x) as u32) // scroll left
            } else {
                (0, 0)
            };
            for _ in 0..count {
                xtest::fake_input(
                    &self.conn,
                    xproto::BUTTON_PRESS_EVENT,
                    button,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                )?;
                xtest::fake_input(
                    &self.conn,
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                )?;
            }
        }

        self.conn.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Button mapping ---

    #[test]
    fn button_left() {
        assert_eq!(InputInjector::map_button(0).unwrap(), 1);
    }

    #[test]
    fn button_middle() {
        assert_eq!(InputInjector::map_button(1).unwrap(), 2);
    }

    #[test]
    fn button_right() {
        assert_eq!(InputInjector::map_button(2).unwrap(), 3);
    }

    #[test]
    fn button_unknown_rejected() {
        assert!(InputInjector::map_button(3).is_err());
        assert!(InputInjector::map_button(255).is_err());
    }

    // --- Scroll accumulation ---

    #[test]
    fn accumulate_scroll_single_full_notch() {
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 1.0);
        assert_eq!(discrete, 1);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_fractional_accumulates() {
        let mut accum = 0.0;
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 1);
        assert!((accum - 0.2).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_negative_direction() {
        let mut accum = 0.0;
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, -1.0), -1);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_large_jump() {
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 5.7);
        assert_eq!(discrete, 5);
        assert!((accum - 0.7).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_preserves_fraction_across_calls() {
        let mut accum = 0.0;
        InputInjector::accumulate_scroll(&mut accum, 0.5);
        assert!((accum - 0.5).abs() < 0.001);
        InputInjector::accumulate_scroll(&mut accum, 0.5);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_direction_change() {
        let mut accum = 0.0;
        InputInjector::accumulate_scroll(&mut accum, 0.5);
        assert!((accum - 0.5).abs() < 0.001);
        InputInjector::accumulate_scroll(&mut accum, -0.5);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_exact_zero_input_returns_zero() {
        // A 0.0 push should be a no-op and leave the accumulator untouched.
        let mut accum = 0.5;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 0.0);
        assert_eq!(discrete, 0);
        assert!((accum - 0.5).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_just_under_one_notch_stays_subnotch() {
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 0.999);
        assert_eq!(discrete, 0);
        assert!((accum - 0.999).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_two_subnotch_pushes_yield_one_notch() {
        let mut accum = 0.0;
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.6), 0);
        // First push leaves 0.6 in the bank.
        let discrete = InputInjector::accumulate_scroll(&mut accum, 0.6);
        assert_eq!(discrete, 1);
        // 0.6 + 0.6 = 1.2 → 1 notch emitted, 0.2 left in the accumulator.
        assert!((accum - 0.2).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_large_negative_jump() {
        // Symmetric to the positive large-jump case: -5.7 → -5 with -0.7 carry.
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, -5.7);
        assert_eq!(discrete, -5);
        assert!((accum - (-0.7)).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_negative_then_positive_returns_to_zero() {
        let mut accum = 0.0;
        let _ = InputInjector::accumulate_scroll(&mut accum, -0.3);
        let _ = InputInjector::accumulate_scroll(&mut accum, 0.3);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_keeps_sign_of_residual() {
        // Negative pushes should leave a negative residual (no sign flip from `as i32`).
        let mut accum = 0.0;
        let _ = InputInjector::accumulate_scroll(&mut accum, -1.4);
        // 1 negative notch emitted, residual is between -1 and 0.
        assert!(accum < 0.0, "residual should be negative, got {accum}");
        assert!(accum > -1.0, "residual should be > -1, got {accum}");
    }

    #[test]
    fn map_button_known_buttons_are_offset_by_one() {
        // Browser button index → X11 button: 0→1, 1→2, 2→3.
        for browser in 0u8..3 {
            let x11 = InputInjector::map_button(browser).unwrap();
            assert_eq!(
                x11,
                browser + 1,
                "Browser button {browser} should map to X11 {}",
                browser + 1
            );
        }
    }

    #[test]
    fn map_button_errors_for_each_unknown_index() {
        // Anything from 3 upwards must fail, not silently map to a stale button.
        for b in [3u8, 4, 5, 10, 100] {
            assert!(
                InputInjector::map_button(b).is_err(),
                "Browser button {b} should be rejected"
            );
        }
    }
}
