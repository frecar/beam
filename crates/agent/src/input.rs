use anyhow::Context;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::info;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

/// Trait over the X11 input-injection methods exposed by [`InputInjector`].
///
/// The production binary uses [`InputInjector`] which owns a live X11
/// connection. Tests need to drive the input-dispatch logic without an X
/// display, so we abstract over the surface the input callback needs and
/// hand around `&mut dyn Inject` instead of `&mut InputInjector`. This lets
/// callbacks be exercised against a mock that records each call.
pub trait Inject: Send {
    /// Inject a keyboard event. `code` is a Linux evdev keycode; the impl
    /// performs the +8 X11 offset internally.
    fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()>;
    /// Inject absolute mouse movement using normalized [0.0, 1.0] coords.
    fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()>;
    /// Inject relative mouse movement (pointer lock mode).
    fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()>;
    /// Inject a mouse-button press/release event. `button` uses the browser
    /// index (0=left, 1=middle, 2=right).
    fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()>;
    /// Inject scroll events. `dx`/`dy` are raw pixel deltas; the impl
    /// accumulates fractional pixels and dispatches discrete X11 notches.
    fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()>;
}

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
        let (event_type, x_keycode) = key_inject_params(code, pressed);
        xtest::fake_input(&self.conn, event_type, x_keycode, 0, self.root, 0, 0, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Inject absolute mouse movement from normalized [0.0, 1.0] coordinates.
    pub fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        let w = self.width.load(Ordering::Relaxed);
        let h = self.height.load(Ordering::Relaxed);
        let (px, py) = mouse_abs_to_pixels(x, y, w, h);
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
        let Some((dx_i, dy_i)) = mouse_rel_round(dx, dy) else {
            return Ok(());
        };
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

    /// Build the X11 keycode from a Linux evdev keycode. X11 keycode =
    /// evdev keycode + 8. Pure helper exposed for unit testing without
    /// owning an X11 connection.
    pub(crate) fn evdev_to_x11_keycode(evdev: u16) -> u8 {
        (evdev + 8) as u8
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

    /// Pixels-per-notch divisor that converts raw scroll deltas into X11
    /// scroll notches. Exposed so callers + tests can share the constant.
    pub(crate) const PIXELS_PER_NOTCH: f64 = 30.0;

    /// Minimum delta magnitude at which the scroll dispatch fires. Anything
    /// at or below this is treated as no movement, avoiding wasted XTEST
    /// round-trips for trackpad jitter.
    pub(crate) const SCROLL_DEADZONE: f64 = 0.001;

    /// Decide the X11 scroll button + notch count for a vertical scroll
    /// delta. Returns `None` for zero or sub-notch movement. The caller
    /// emits `count` press+release pairs of `button` via XTestFakeInput.
    ///
    /// Pure helper so the dispatch logic can be unit-tested without owning
    /// an X11 connection.
    pub(crate) fn vertical_scroll_button(discrete: i32) -> Option<(u8, u32)> {
        if discrete > 0 {
            Some((4u8, discrete as u32))
        } else if discrete < 0 {
            Some((5u8, discrete.unsigned_abs()))
        } else {
            None
        }
    }

    /// Decide the X11 scroll button + notch count for a horizontal scroll
    /// delta. Returns `None` for zero or sub-notch movement.
    pub(crate) fn horizontal_scroll_button(discrete: i32) -> Option<(u8, u32)> {
        if discrete > 0 {
            Some((7u8, discrete as u32))
        } else if discrete < 0 {
            Some((6u8, discrete.unsigned_abs()))
        } else {
            None
        }
    }

    /// Decide whether a raw scroll delta is large enough to dispatch. Pure
    /// helper that matches the production dead-zone (0.001 pixel).
    pub(crate) fn scroll_delta_is_active(delta: f64) -> bool {
        delta.abs() > Self::SCROLL_DEADZONE
    }

    /// Inject scroll events.
    /// X11 scroll uses button 4/5 (vertical) and 6/7 (horizontal).
    /// Each scroll notch is a press+release of the corresponding button.
    pub fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        // Vertical scroll: button 4 = up, button 5 = down
        if Self::scroll_delta_is_active(dy) {
            let discrete_y =
                Self::accumulate_scroll(&mut self.scroll_accum_y, -dy / Self::PIXELS_PER_NOTCH);
            if let Some((button, count)) = Self::vertical_scroll_button(discrete_y) {
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
        }

        // Horizontal scroll: button 6 = left, button 7 = right
        if Self::scroll_delta_is_active(dx) {
            let discrete_x =
                Self::accumulate_scroll(&mut self.scroll_accum_x, dx / Self::PIXELS_PER_NOTCH);
            if let Some((button, count)) = Self::horizontal_scroll_button(discrete_x) {
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
        }

        self.conn.flush()?;
        Ok(())
    }
}

/// Compute the `(event_type, x_keycode)` tuple passed to XTestFakeInput
/// for a keyboard event. Pure helper so the offset + press/release
/// classification is testable without an X connection.
pub(crate) fn key_inject_params(evdev_code: u16, pressed: bool) -> (u8, u8) {
    let x_keycode = InputInjector::evdev_to_x11_keycode(evdev_code);
    let event_type = if pressed {
        xproto::KEY_PRESS_EVENT
    } else {
        xproto::KEY_RELEASE_EVENT
    };
    (event_type, x_keycode)
}

/// Convert normalized [0.0, 1.0] mouse coords to pixel coordinates inside
/// the current viewport. Values outside the bounds are clamped, so the
/// produced i16 always falls inside [0, width or height].
pub(crate) fn mouse_abs_to_pixels(x: f64, y: f64, width: u32, height: u32) -> (i16, i16) {
    let px = (x.clamp(0.0, 1.0) * width as f64) as i16;
    let py = (y.clamp(0.0, 1.0) * height as f64) as i16;
    (px, py)
}

/// Round (dx, dy) to integer pixels. Returns `None` if both rounded
/// deltas are zero — in that case the caller should skip the inject.
pub(crate) fn mouse_rel_round(dx: f64, dy: f64) -> Option<(i16, i16)> {
    let dx_i = dx.round() as i16;
    let dy_i = dy.round() as i16;
    if dx_i == 0 && dy_i == 0 {
        None
    } else {
        Some((dx_i, dy_i))
    }
}

impl Inject for InputInjector {
    fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        InputInjector::inject_key(self, code, pressed)
    }

    fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        InputInjector::inject_mouse_move_abs(self, x, y)
    }

    fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        InputInjector::inject_mouse_move_rel(self, dx, dy)
    }

    fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        InputInjector::inject_button(self, button, pressed)
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        InputInjector::inject_scroll(self, dx, dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Button mapping ---

    // --- key_inject_params ---

    #[test]
    fn key_inject_params_press_is_key_press_event() {
        let (event_type, x_keycode) = key_inject_params(30, true);
        assert_eq!(event_type, xproto::KEY_PRESS_EVENT);
        assert_eq!(x_keycode, 38); // 30 + 8
    }

    #[test]
    fn key_inject_params_release_is_key_release_event() {
        let (event_type, x_keycode) = key_inject_params(30, false);
        assert_eq!(event_type, xproto::KEY_RELEASE_EVENT);
        assert_eq!(x_keycode, 38);
    }

    #[test]
    fn key_inject_params_applies_evdev_offset() {
        let (_, x_keycode) = key_inject_params(0, true);
        assert_eq!(x_keycode, 8);
    }

    #[test]
    fn key_inject_params_max_u8_keycode() {
        // 247 + 8 = 255 = u8::MAX
        let (_, x_keycode) = key_inject_params(247, true);
        assert_eq!(x_keycode, 255);
    }

    // --- mouse_abs_to_pixels ---

    #[test]
    fn mouse_abs_to_pixels_center() {
        let (px, py) = mouse_abs_to_pixels(0.5, 0.5, 1920, 1080);
        assert_eq!(px, 960);
        assert_eq!(py, 540);
    }

    #[test]
    fn mouse_abs_to_pixels_top_left() {
        let (px, py) = mouse_abs_to_pixels(0.0, 0.0, 1920, 1080);
        assert_eq!(px, 0);
        assert_eq!(py, 0);
    }

    #[test]
    fn mouse_abs_to_pixels_bottom_right() {
        let (px, py) = mouse_abs_to_pixels(1.0, 1.0, 1920, 1080);
        assert_eq!(px, 1920);
        assert_eq!(py, 1080);
    }

    #[test]
    fn mouse_abs_to_pixels_clamps_out_of_range_high() {
        let (px, py) = mouse_abs_to_pixels(2.0, 5.0, 1920, 1080);
        assert_eq!(px, 1920);
        assert_eq!(py, 1080);
    }

    #[test]
    fn mouse_abs_to_pixels_clamps_out_of_range_low() {
        let (px, py) = mouse_abs_to_pixels(-1.5, -10.0, 1920, 1080);
        assert_eq!(px, 0);
        assert_eq!(py, 0);
    }

    #[test]
    fn mouse_abs_to_pixels_small_viewport() {
        let (px, py) = mouse_abs_to_pixels(0.5, 0.5, 800, 600);
        assert_eq!(px, 400);
        assert_eq!(py, 300);
    }

    // --- mouse_rel_round ---

    #[test]
    fn mouse_rel_round_nonzero_returns_some() {
        let result = mouse_rel_round(1.4, -2.6);
        assert_eq!(result, Some((1, -3)));
    }

    #[test]
    fn mouse_rel_round_zero_returns_none() {
        assert_eq!(mouse_rel_round(0.0, 0.0), None);
    }

    #[test]
    fn mouse_rel_round_sub_half_returns_none() {
        // 0.4 rounds to 0, 0.4 rounds to 0 → None
        assert_eq!(mouse_rel_round(0.4, 0.4), None);
    }

    #[test]
    fn mouse_rel_round_one_zero_one_nonzero() {
        // dx rounds to nonzero, dy rounds to zero → Some
        let r = mouse_rel_round(1.0, 0.0);
        assert_eq!(r, Some((1, 0)));
    }

    #[test]
    fn mouse_rel_round_zero_then_nonzero() {
        let r = mouse_rel_round(0.0, -5.5);
        assert_eq!(r, Some((0, -6)));
    }

    #[test]
    fn mouse_rel_round_at_negative_boundary_returns_some() {
        // -0.5 rounds away from zero per Rust's f64::round() semantics
        let r = mouse_rel_round(-0.6, 0.0);
        assert_eq!(r, Some((-1, 0)));
    }

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

    // --- Constructor failure path ---

    #[test]
    fn input_injector_new_rejects_bogus_display() {
        // Connecting to a clearly-unreachable display must return an error
        // (not panic). This exercises the `?` in RustConnection::connect.
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));
        let result = InputInjector::new(":99999", Arc::clone(&width), Arc::clone(&height));
        assert!(result.is_err(), "Bogus display should fail to connect");
    }

    #[test]
    fn input_injector_new_rejects_empty_display() {
        // Empty display string also fails (no socket path).
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));
        let result = InputInjector::new("", Arc::clone(&width), Arc::clone(&height));
        assert!(result.is_err(), "Empty display should fail to connect");
    }

    // --- Scroll accumulator: extreme inputs ---

    #[test]
    fn accumulate_scroll_handles_very_large_positive() {
        // 1000 pixels of scroll should yield 1000 notches.
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 1000.0);
        assert_eq!(discrete, 1000);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_handles_very_large_negative() {
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, -1000.0);
        assert_eq!(discrete, -1000);
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_converges_after_thousand_subnotch_pushes() {
        // Stress: 1000 pushes of 0.001 = 1.000 total, which should produce
        // exactly 1 notch with residue close to zero.
        let mut accum = 0.0;
        let mut total = 0i32;
        for _ in 0..1000 {
            total += InputInjector::accumulate_scroll(&mut accum, 0.001);
        }
        assert_eq!(total, 1);
        assert!(accum.abs() < 0.01);
    }

    #[test]
    fn accumulate_scroll_zero_input_with_positive_residue() {
        // Zero input should NOT clear the existing residue.
        let mut accum = 0.4;
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.0), 0);
        assert!((accum - 0.4).abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_pixel_to_notch_division() {
        // Calling sites use `dy / 30.0` to convert pixels-to-notches. Verify
        // that exactly 30 pixels produce 1 notch.
        let mut accum = 0.0;
        let discrete = InputInjector::accumulate_scroll(&mut accum, 30.0 / 30.0);
        assert_eq!(discrete, 1);
    }

    // --- map_button: full boundary ---

    #[test]
    fn map_button_rejects_u8_max() {
        assert!(InputInjector::map_button(u8::MAX).is_err());
    }

    #[test]
    fn map_button_rejects_three_specifically() {
        // 3 is the first invalid value (the next one after 2=right).
        // Browser events never produce 3+, but defensive check.
        let err = InputInjector::map_button(3).unwrap_err();
        assert!(
            err.to_string().contains('3'),
            "Error must mention the bad index"
        );
    }

    // --- Scroll dispatch helpers (vertical / horizontal button selection) ---

    #[test]
    fn vertical_scroll_positive_yields_button_4() {
        // Positive discrete = scroll up = X11 button 4.
        let (button, count) = InputInjector::vertical_scroll_button(1).unwrap();
        assert_eq!(button, 4);
        assert_eq!(count, 1);
    }

    #[test]
    fn vertical_scroll_negative_yields_button_5() {
        // Negative discrete = scroll down = X11 button 5.
        let (button, count) = InputInjector::vertical_scroll_button(-3).unwrap();
        assert_eq!(button, 5);
        assert_eq!(count, 3, "Count must be the absolute value, not negative");
    }

    #[test]
    fn vertical_scroll_zero_yields_none() {
        assert!(InputInjector::vertical_scroll_button(0).is_none());
    }

    #[test]
    fn vertical_scroll_large_magnitude() {
        let (button, count) = InputInjector::vertical_scroll_button(120).unwrap();
        assert_eq!(button, 4);
        assert_eq!(count, 120);
        let (button, count) = InputInjector::vertical_scroll_button(-120).unwrap();
        assert_eq!(button, 5);
        assert_eq!(count, 120);
    }

    #[test]
    fn vertical_scroll_imin_does_not_panic() {
        // i32::MIN can panic with naive `-x as u32` (signed overflow).
        // `unsigned_abs` is the safe form — verify the helper handles it.
        let (button, count) = InputInjector::vertical_scroll_button(i32::MIN).unwrap();
        assert_eq!(button, 5);
        assert_eq!(count, i32::MIN.unsigned_abs());
    }

    #[test]
    fn horizontal_scroll_positive_yields_button_7() {
        // Positive discrete = scroll right = X11 button 7.
        let (button, count) = InputInjector::horizontal_scroll_button(1).unwrap();
        assert_eq!(button, 7);
        assert_eq!(count, 1);
    }

    #[test]
    fn horizontal_scroll_negative_yields_button_6() {
        // Negative discrete = scroll left = X11 button 6.
        let (button, count) = InputInjector::horizontal_scroll_button(-5).unwrap();
        assert_eq!(button, 6);
        assert_eq!(count, 5);
    }

    #[test]
    fn horizontal_scroll_zero_yields_none() {
        assert!(InputInjector::horizontal_scroll_button(0).is_none());
    }

    #[test]
    fn horizontal_scroll_imin_does_not_panic() {
        let (button, count) = InputInjector::horizontal_scroll_button(i32::MIN).unwrap();
        assert_eq!(button, 6);
        assert_eq!(count, i32::MIN.unsigned_abs());
    }

    // --- Scroll deadzone ---

    #[test]
    fn scroll_delta_active_above_deadzone() {
        assert!(InputInjector::scroll_delta_is_active(0.002));
        assert!(InputInjector::scroll_delta_is_active(-0.002));
        assert!(InputInjector::scroll_delta_is_active(30.0));
        assert!(InputInjector::scroll_delta_is_active(-30.0));
    }

    #[test]
    fn scroll_delta_inactive_at_or_below_deadzone() {
        // Strict `> 0.001`, so 0.001 itself is inactive.
        assert!(!InputInjector::scroll_delta_is_active(0.0));
        assert!(!InputInjector::scroll_delta_is_active(0.001));
        assert!(!InputInjector::scroll_delta_is_active(-0.001));
        assert!(!InputInjector::scroll_delta_is_active(0.0001));
        assert!(!InputInjector::scroll_delta_is_active(-0.0001));
    }

    #[test]
    fn scroll_pixels_per_notch_locks_to_30() {
        // The constant pins the "30 pixels = 1 notch" UX agreed across the
        // browser + agent. Changing it without a paired browser update
        // produces visibly different scroll speed.
        assert_eq!(InputInjector::PIXELS_PER_NOTCH, 30.0);
    }

    #[test]
    fn scroll_deadzone_locks_to_one_thousandth() {
        // Browsers occasionally emit residual sub-pixel deltas (~1e-5);
        // the dead-zone must be small enough that intentional scrolls
        // (≥0.01) always fire, but big enough that noise doesn't.
        assert_eq!(InputInjector::SCROLL_DEADZONE, 0.001);
    }

    #[test]
    fn vertical_scroll_imax_does_not_overflow() {
        // i32::MAX as u32 is safe (no sign issue); verify the cast holds.
        let (button, count) = InputInjector::vertical_scroll_button(i32::MAX).unwrap();
        assert_eq!(button, 4);
        assert_eq!(count, i32::MAX as u32);
    }

    #[test]
    fn horizontal_scroll_imax_does_not_overflow() {
        let (button, count) = InputInjector::horizontal_scroll_button(i32::MAX).unwrap();
        assert_eq!(button, 7);
        assert_eq!(count, i32::MAX as u32);
    }
}
