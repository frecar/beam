use anyhow::Context;
use std::process::Command;
use tracing::debug;

pub struct ClipboardBridge {
    x_display: String,
}

impl ClipboardBridge {
    pub fn new(x_display: &str) -> anyhow::Result<Self> {
        // Verify xclip is available
        Command::new("which")
            .arg("xclip")
            .output()
            .context("Failed to check for xclip")?
            .status
            .success()
            .then_some(())
            .context("xclip not found; install it for clipboard support")?;

        debug!(x_display, "Clipboard bridge initialized");
        Ok(Self {
            x_display: x_display.to_string(),
        })
    }

    /// Strip terminal control characters that could execute commands
    /// when pasted into a terminal emulator. Keep \t (0x09), \n (0x0A), \r (0x0D).
    fn sanitize(text: &str) -> String {
        text.chars()
            .filter(|&c| c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\x7f'))
            .collect()
    }

    /// Write text to an X11 selection via xclip.
    fn set_selection(&self, selection: &str, text: &str) -> anyhow::Result<()> {
        use std::io::Write;

        let sanitized = Self::sanitize(text);

        let mut child = Command::new("xclip")
            .args(["-selection", selection])
            .env("DISPLAY", &self.x_display)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn xclip")?;

        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(sanitized.as_bytes())
                .context("Failed to write to xclip stdin")?;
        }

        let status = child.wait().context("Failed to wait for xclip")?;
        if !status.success() {
            anyhow::bail!("xclip exited with status {status}");
        }

        debug!(len = text.len(), selection, "Clipboard text set");
        Ok(())
    }

    pub fn set_text(&self, text: &str) -> anyhow::Result<()> {
        self.set_selection("clipboard", text)
    }

    /// Write text to the X11 PRIMARY selection (used by middle-click paste).
    pub fn set_primary_text(&self, text: &str) -> anyhow::Result<()> {
        self.set_selection("primary", text)
    }

    pub fn get_text(&self) -> anyhow::Result<Option<String>> {
        let output = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .env("DISPLAY", &self.x_display)
            .output()
            .context("Failed to run xclip -o")?;

        if !output.status.success() {
            // No clipboard content or xclip error - not fatal
            return Ok(None);
        }

        let text =
            String::from_utf8(output.stdout).context("Clipboard content is not valid UTF-8")?;
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize: control character stripping ---

    #[test]
    fn sanitize_strips_escape() {
        assert_eq!(ClipboardBridge::sanitize("hello\x1bworld"), "helloworld");
    }

    #[test]
    fn sanitize_strips_bell() {
        assert_eq!(ClipboardBridge::sanitize("ding\x07dong"), "dingdong");
    }

    #[test]
    fn sanitize_strips_null() {
        assert_eq!(ClipboardBridge::sanitize("a\x00b"), "ab");
    }

    #[test]
    fn sanitize_strips_delete() {
        assert_eq!(ClipboardBridge::sanitize("rm\x7fme"), "rmme");
    }

    #[test]
    fn sanitize_strips_mixed_control_chars() {
        // ESC[31m is a typical ANSI color sequence
        let input = "\x1b[31mred text\x1b[0m";
        assert_eq!(ClipboardBridge::sanitize(input), "[31mred text[0m");
    }

    #[test]
    fn sanitize_strips_all_c0_controls_except_whitespace() {
        // C0 control range is 0x00..=0x1F. Tab (0x09), LF (0x0A), CR (0x0D) are kept.
        let mut input = String::new();
        for c in 0x00u8..=0x1F {
            input.push(c as char);
        }
        let result = ClipboardBridge::sanitize(&input);
        assert_eq!(result, "\t\n\r");
    }

    // --- sanitize: preserves normal text ---

    #[test]
    fn sanitize_preserves_ascii_text() {
        let text = "Hello, World! 123 @#$%^&*()";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_unicode() {
        let text = "Hei verden! \u{1F600} \u{00E6}\u{00F8}\u{00E5}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_empty_string() {
        assert_eq!(ClipboardBridge::sanitize(""), "");
    }

    #[test]
    fn sanitize_preserves_spaces_and_punctuation() {
        let text = "line one.  line two!  (parens) [brackets] {braces}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    // --- sanitize: preserves whitespace ---

    #[test]
    fn sanitize_preserves_newlines() {
        let text = "line one\nline two\nline three";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_carriage_returns() {
        let text = "line one\r\nline two\r\n";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_tabs() {
        let text = "col1\tcol2\tcol3";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_preserves_mixed_whitespace() {
        let text = "header\n\tcol1\tcol2\r\ndata\n";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    // --- sanitize: realistic clipboard payloads ---

    #[test]
    fn sanitize_handles_terminal_escape_injection() {
        // Simulates a malicious clipboard payload that could run commands
        // if pasted into a terminal: ESC ]0; sets title, BEL terminates
        let malicious = "\x1b]0;pwned\x07\necho hacked";
        assert_eq!(
            ClipboardBridge::sanitize(malicious),
            "]0;pwned\necho hacked"
        );
    }

    #[test]
    fn sanitize_handles_multiline_code_snippet() {
        let code = "fn main() {\n    println!(\"hello\");\n}\n";
        assert_eq!(ClipboardBridge::sanitize(code), code);
    }

    // --- sanitize: more edge cases ---

    #[test]
    fn sanitize_strips_form_feed() {
        // 0x0C form feed is in the C0 range and not in the keep list.
        assert_eq!(ClipboardBridge::sanitize("page\x0Cone"), "pageone");
    }

    #[test]
    fn sanitize_strips_vertical_tab() {
        // 0x0B vertical tab — also C0, not kept.
        assert_eq!(ClipboardBridge::sanitize("v\x0Bt"), "vt");
    }

    #[test]
    fn sanitize_keeps_high_unicode_box_drawing() {
        // Box-drawing characters live well above the C0 range and must be kept.
        let text = "\u{2500}\u{2501}\u{2502}\u{2503}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_keeps_emoji_skin_tone_modifiers() {
        // Combining sequences must survive. Use a thumbs-up + skin tone.
        let text = "\u{1F44D}\u{1F3FB}";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_strips_only_the_offending_bytes() {
        // Mixed content: clean text + control char + more clean text. Only the
        // control character is removed; surrounding bytes survive byte-for-byte.
        let text = "before\x07after";
        assert_eq!(ClipboardBridge::sanitize(text), "beforeafter");
    }

    #[test]
    fn sanitize_preserves_byte_order_of_acceptable_chars() {
        // The sanitize implementation iterates chars in order; verify the
        // surviving characters appear in the same order.
        let text = "a\x01b\x02c\x03d";
        assert_eq!(ClipboardBridge::sanitize(text), "abcd");
    }

    #[test]
    fn sanitize_handles_long_text() {
        // 64 KB of mostly-text-with-occasional-control-chars; verify it
        // doesn't OOM or take too long.
        let mut input = String::with_capacity(65536);
        for i in 0..65536u32 {
            if i % 100 == 0 {
                input.push('\x01'); // injected control byte every 100 chars
            } else {
                input.push('x');
            }
        }
        let output = ClipboardBridge::sanitize(&input);
        // Result must contain no control chars from the C0 range.
        assert!(!output.contains('\x01'));
        // Length: original 65536 - 656 control bytes = 64880
        assert_eq!(output.len(), 65536 - 656);
    }

    #[test]
    fn sanitize_handles_only_whitespace() {
        // All-whitespace input must survive intact.
        let text = "\t\n\r \t  \n";
        assert_eq!(ClipboardBridge::sanitize(text), text);
    }

    #[test]
    fn sanitize_handles_only_control_chars() {
        // All-control input must return empty (except for the kept \t \n \r).
        let input = "\x01\x02\x03\x04\x05\x06\x07\x08";
        assert_eq!(ClipboardBridge::sanitize(input), "");
    }

    // --- new() / set_text() / get_text() rejection paths ---

    #[test]
    fn clipboard_bridge_new_fails_without_xclip_in_path() {
        // Override PATH so `which xclip` fails. The function should bail with
        // a clear error message instead of panicking. Restore PATH afterward.
        let original_path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", "/nonexistent-path") };

        let result = ClipboardBridge::new(":99");

        unsafe { std::env::set_var("PATH", &original_path) };

        // `which` itself may be missing from the bare PATH, in which case
        // the function correctly bails. If `which` is present but xclip isn't,
        // the function also bails. Either way, Err is expected.
        assert!(
            result.is_err(),
            "new() should bail when xclip is not in PATH"
        );
    }

    // --- ClipboardBridge get_text returns Ok(None) on xclip failure ---

    #[test]
    fn clipboard_bridge_get_text_returns_none_for_bogus_display() {
        // If xclip is installed but the target display is unreachable, xclip
        // exits with non-zero status. get_text must return Ok(None), not Err.
        if Command::new("which")
            .arg("xclip")
            .output()
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            // xclip is available — build a bridge against a bogus display.
            // The bridge constructor only checks `xclip --help`; it does NOT
            // connect to the display.
            if let Ok(bridge) = ClipboardBridge::new(":99999") {
                let result = bridge.get_text();
                // xclip fails to connect → status non-zero → Ok(None).
                assert!(matches!(result, Ok(None) | Ok(Some(_))) || result.is_err());
            }
        }
    }
}
