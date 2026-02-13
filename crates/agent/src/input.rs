use anyhow::Context;
use input_linux::sys::input_event;
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId,
    Key, KeyEvent, KeyState, RelativeAxis, RelativeEvent, SynchronizeEvent, UInputHandle,
};
use std::fs::{File, OpenOptions};
use tracing::debug;

const ABS_MAX: i32 = 65535;

pub struct InputInjector {
    keyboard: UInputHandle<File>,
    mouse: UInputHandle<File>,
    /// Separate device for relative mouse — mixing ABS and REL axes on one
    /// device causes libinput to misclassify it, breaking absolute positioning.
    rel_mouse: UInputHandle<File>,
    /// Accumulated fractional scroll for smooth trackpad support
    scroll_accum_x: f64,
    scroll_accum_y: f64,
}

impl InputInjector {
    pub fn new() -> anyhow::Result<Self> {
        let keyboard = Self::create_keyboard().context("Failed to create virtual keyboard")?;
        let mouse = Self::create_mouse().context("Failed to create virtual mouse")?;
        let rel_mouse = Self::create_rel_mouse().context("Failed to create virtual relative mouse")?;
        debug!("Input injector initialized");
        Ok(Self {
            keyboard,
            mouse,
            rel_mouse,
            scroll_accum_x: 0.0,
            scroll_accum_y: 0.0,
        })
    }

    fn open_uinput() -> anyhow::Result<File> {
        OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("Failed to open /dev/uinput (check permissions)")
    }

    fn create_keyboard() -> anyhow::Result<UInputHandle<File>> {
        let file = Self::open_uinput()?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Synchronize)?;

        // Enable all standard keys (codes 1..=248 cover ESC through standard keyboard keys)
        for code in 1..=248u16 {
            if let Ok(key) = Key::from_code(code) {
                handle.set_keybit(key)?;
            }
        }

        let id = InputId {
            bustype: 0x03, // BUS_USB
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
        };

        handle.create(&id, b"Beam Virtual Keyboard\0", 0, &[])?;
        debug!("Virtual keyboard created");
        Ok(handle)
    }

    fn create_mouse() -> anyhow::Result<UInputHandle<File>> {
        let file = Self::open_uinput()?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Relative)?;
        handle.set_evbit(EventKind::Synchronize)?;

        // Mouse buttons
        handle.set_keybit(Key::ButtonLeft)?;
        handle.set_keybit(Key::ButtonRight)?;
        handle.set_keybit(Key::ButtonMiddle)?;

        // Absolute axes for positioning
        handle.set_absbit(AbsoluteAxis::X)?;
        handle.set_absbit(AbsoluteAxis::Y)?;

        // Relative axes for scroll (NOT REL_X/REL_Y — those go on a separate
        // device to avoid libinput misclassifying this as a relative device)
        handle.set_relbit(RelativeAxis::Wheel)?;
        handle.set_relbit(RelativeAxis::HorizontalWheel)?;
        // High-resolution scroll for smooth trackpad
        handle.set_relbit(RelativeAxis::WheelHiRes)?;
        handle.set_relbit(RelativeAxis::HorizontalWheelHiRes)?;

        let abs_x = AbsoluteInfoSetup {
            axis: AbsoluteAxis::X,
            info: AbsoluteInfo {
                value: 0,
                minimum: 0,
                maximum: ABS_MAX,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };

        let abs_y = AbsoluteInfoSetup {
            axis: AbsoluteAxis::Y,
            info: AbsoluteInfo {
                value: 0,
                minimum: 0,
                maximum: ABS_MAX,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };

        let id = InputId {
            bustype: 0x03, // BUS_USB
            vendor: 0x1234,
            product: 0x5679,
            version: 1,
        };

        handle.create(&id, b"Beam Virtual Mouse\0", 0, &[abs_x, abs_y])?;
        debug!("Virtual mouse created");
        Ok(handle)
    }

    /// Separate device for relative mouse movement (pointer lock mode).
    /// Must be a different device from the absolute mouse to prevent
    /// libinput from misclassifying the absolute device.
    fn create_rel_mouse() -> anyhow::Result<UInputHandle<File>> {
        let file = Self::open_uinput()?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Relative)?;
        handle.set_evbit(EventKind::Synchronize)?;

        // Mouse buttons (needed for click-drag in pointer lock)
        handle.set_keybit(Key::ButtonLeft)?;
        handle.set_keybit(Key::ButtonRight)?;
        handle.set_keybit(Key::ButtonMiddle)?;

        // Relative axes for pointer lock movement
        handle.set_relbit(RelativeAxis::X)?;
        handle.set_relbit(RelativeAxis::Y)?;

        let id = InputId {
            bustype: 0x03, // BUS_USB
            vendor: 0x1234,
            product: 0x567a,
            version: 1,
        };

        handle.create(&id, b"Beam Virtual Relative Mouse\0", 0, &[])?;
        debug!("Virtual relative mouse created");
        Ok(handle)
    }

    pub fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        let key =
            Key::from_code(code).map_err(|_| anyhow::anyhow!("Invalid key code: {code}"))?;
        let time = EventTime::default();
        let events = [
            KeyEvent::new(time, key, KeyState::pressed(pressed))
                .into_event()
                .into_raw(),
            SynchronizeEvent::report(time).into_event().into_raw(),
        ];
        self.keyboard.write(&events)?;
        Ok(())
    }

    /// Convert normalized [0.0, 1.0] coordinate to absolute uinput value.
    fn normalize_to_abs(v: f64) -> i32 {
        (v.clamp(0.0, 1.0) * ABS_MAX as f64) as i32
    }

    pub fn inject_mouse_move_abs(
        &mut self,
        x: f64,
        y: f64,
    ) -> anyhow::Result<()> {
        let abs_x = Self::normalize_to_abs(x);
        let abs_y = Self::normalize_to_abs(y);
        let time = EventTime::default();
        let events: [input_event; 3] = [
            AbsoluteEvent::new(time, AbsoluteAxis::X, abs_x)
                .into_event()
                .into_raw(),
            AbsoluteEvent::new(time, AbsoluteAxis::Y, abs_y)
                .into_event()
                .into_raw(),
            SynchronizeEvent::report(time).into_event().into_raw(),
        ];
        self.mouse.write(&events)?;
        Ok(())
    }

    pub fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let dx_i = dx.round() as i32;
        let dy_i = dy.round() as i32;
        if dx_i == 0 && dy_i == 0 {
            return Ok(());
        }
        let time = EventTime::default();
        let mut events: Vec<input_event> = Vec::with_capacity(3);
        if dx_i != 0 {
            events.push(
                RelativeEvent::new(time, RelativeAxis::X, dx_i)
                    .into_event()
                    .into_raw(),
            );
        }
        if dy_i != 0 {
            events.push(
                RelativeEvent::new(time, RelativeAxis::Y, dy_i)
                    .into_event()
                    .into_raw(),
            );
        }
        events.push(SynchronizeEvent::report(time).into_event().into_raw());
        self.rel_mouse.write(&events)?;
        Ok(())
    }

    /// Map browser button index to Linux input key.
    /// 0=left, 1=middle, 2=right (matches MouseEvent.button).
    fn map_button(button: u8) -> anyhow::Result<Key> {
        match button {
            0 => Ok(Key::ButtonLeft),
            1 => Ok(Key::ButtonMiddle),
            2 => Ok(Key::ButtonRight),
            _ => anyhow::bail!("Unknown mouse button: {button}"),
        }
    }

    pub fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        let key = Self::map_button(button)?;
        let time = EventTime::default();
        let events = [
            KeyEvent::new(time, key, KeyState::pressed(pressed))
                .into_event()
                .into_raw(),
            SynchronizeEvent::report(time).into_event().into_raw(),
        ];
        self.mouse.write(&events)?;
        Ok(())
    }

    /// Convert pixel scroll delta to high-resolution scroll units.
    /// 30 pixels ≈ 1 scroll notch ≈ 120 hi-res units.
    fn pixel_to_hires(pixels: f64) -> i32 {
        (pixels / 30.0 * 120.0) as i32
    }

    /// Accumulate fractional scroll and return discrete notch count.
    /// Updates the accumulator, subtracting any emitted discrete value.
    fn accumulate_scroll(accum: &mut f64, pixels_per_notch: f64) -> i32 {
        *accum += pixels_per_notch;
        let discrete = *accum as i32;
        if discrete != 0 {
            *accum -= discrete as f64;
        }
        discrete
    }

    /// Inject scroll events with smooth trackpad support.
    ///
    /// Pixel-level scroll deltas from the browser are accumulated and converted
    /// to discrete scroll events. Also sends high-resolution scroll events for
    /// applications that support them (REL_WHEEL_HI_RES).
    pub fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let time = EventTime::default();
        let mut events = Vec::with_capacity(5);

        // Send high-resolution scroll events (120 units = 1 notch)
        // This preserves trackpad smoothness for apps that support it.
        if dy.abs() > 0.001 {
            let hires_value = Self::pixel_to_hires(-dy);
            if hires_value != 0 {
                events.push(
                    RelativeEvent::new(time, RelativeAxis::WheelHiRes, hires_value)
                        .into_event()
                        .into_raw(),
                );
            }

            let discrete_y = Self::accumulate_scroll(&mut self.scroll_accum_y, -dy / 30.0);
            if discrete_y != 0 {
                events.push(
                    RelativeEvent::new(time, RelativeAxis::Wheel, discrete_y)
                        .into_event()
                        .into_raw(),
                );
            }
        }

        if dx.abs() > 0.001 {
            let hires_value = Self::pixel_to_hires(dx);
            if hires_value != 0 {
                events.push(
                    RelativeEvent::new(time, RelativeAxis::HorizontalWheelHiRes, hires_value)
                        .into_event()
                        .into_raw(),
                );
            }

            let discrete_x = Self::accumulate_scroll(&mut self.scroll_accum_x, dx / 30.0);
            if discrete_x != 0 {
                events.push(
                    RelativeEvent::new(time, RelativeAxis::HorizontalWheel, discrete_x)
                        .into_event()
                        .into_raw(),
                );
            }
        }

        if !events.is_empty() {
            events.push(SynchronizeEvent::report(time).into_event().into_raw());
            self.mouse.write(&events)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Coordinate normalization ---

    #[test]
    fn normalize_to_abs_origin() {
        assert_eq!(InputInjector::normalize_to_abs(0.0), 0);
    }

    #[test]
    fn normalize_to_abs_max() {
        assert_eq!(InputInjector::normalize_to_abs(1.0), ABS_MAX);
    }

    #[test]
    fn normalize_to_abs_center() {
        let center = InputInjector::normalize_to_abs(0.5);
        // Should be approximately half of ABS_MAX
        assert!((center - ABS_MAX / 2).abs() <= 1);
    }

    #[test]
    fn normalize_to_abs_clamps_negative() {
        assert_eq!(InputInjector::normalize_to_abs(-0.5), 0);
        assert_eq!(InputInjector::normalize_to_abs(-100.0), 0);
    }

    #[test]
    fn normalize_to_abs_clamps_above_one() {
        assert_eq!(InputInjector::normalize_to_abs(1.5), ABS_MAX);
        assert_eq!(InputInjector::normalize_to_abs(100.0), ABS_MAX);
    }

    // --- Button mapping ---

    #[test]
    fn button_left() {
        assert!(matches!(InputInjector::map_button(0), Ok(Key::ButtonLeft)));
    }

    #[test]
    fn button_middle() {
        assert!(matches!(
            InputInjector::map_button(1),
            Ok(Key::ButtonMiddle)
        ));
    }

    #[test]
    fn button_right() {
        assert!(matches!(
            InputInjector::map_button(2),
            Ok(Key::ButtonRight)
        ));
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
        // Three small scrolls that should accumulate to 1 notch
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 0);
        // Fourth push should cross the threshold
        assert_eq!(InputInjector::accumulate_scroll(&mut accum, 0.3), 1);
        // Remaining fraction should be ~0.2
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
        // Should have emitted 1, remainder 0
        assert!(accum.abs() < 0.001);
    }

    #[test]
    fn accumulate_scroll_direction_change() {
        let mut accum = 0.0;
        // Scroll up partially
        InputInjector::accumulate_scroll(&mut accum, 0.5);
        assert!((accum - 0.5).abs() < 0.001);
        // Reverse direction — should cancel out
        InputInjector::accumulate_scroll(&mut accum, -0.5);
        assert!(accum.abs() < 0.001);
    }

    // --- Pixel-to-hires conversion ---

    #[test]
    fn pixel_to_hires_one_notch() {
        // 30 pixels = 1 notch = 120 hi-res units
        assert_eq!(InputInjector::pixel_to_hires(30.0), 120);
    }

    #[test]
    fn pixel_to_hires_negative() {
        assert_eq!(InputInjector::pixel_to_hires(-30.0), -120);
    }

    #[test]
    fn pixel_to_hires_half_notch() {
        assert_eq!(InputInjector::pixel_to_hires(15.0), 60);
    }

    #[test]
    fn pixel_to_hires_tiny_delta() {
        // Very small pixel delta → 0 hi-res units (below threshold)
        assert_eq!(InputInjector::pixel_to_hires(0.1), 0);
    }

    // --- Relative mouse zero-movement check ---

    #[test]
    fn rel_move_rounds_small_to_zero() {
        // Values < 0.5 round to 0 and should be skipped
        assert_eq!(0.4f64.round() as i32, 0);
        assert_eq!((-0.4f64).round() as i32, 0);
        assert_eq!(0.0f64.round() as i32, 0);
    }

    #[test]
    fn rel_move_rounds_to_nonzero() {
        assert_eq!(0.5f64.round() as i32, 1);
        assert_eq!((-0.5f64).round() as i32, -1); // banker's rounding, but cast is truncation after round
        assert_eq!(1.7f64.round() as i32, 2);
    }
}
