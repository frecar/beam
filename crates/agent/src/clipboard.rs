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

    pub fn set_text(&self, text: &str) -> anyhow::Result<()> {
        use std::io::Write;

        // Strip terminal control characters that could execute commands
        // when pasted into a terminal emulator. Keep \t (0x09), \n (0x0A), \r (0x0D).
        let sanitized: String = text
            .chars()
            .filter(|&c| c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\x7f'))
            .collect();

        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
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

        debug!(len = text.len(), "Clipboard text set");
        Ok(())
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

        let text = String::from_utf8(output.stdout)
            .context("Clipboard content is not valid UTF-8")?;
        Ok(Some(text))
    }
}
