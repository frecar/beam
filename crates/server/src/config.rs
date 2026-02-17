use std::path::Path;

use anyhow::{Context, Result};
use beam_protocol::BeamConfig;

pub use beam_protocol::{AudioConfig, ServerConfig, SessionConfig, VideoConfig};

/// Load configuration from a TOML file at the given path.
/// If the file doesn't exist, returns default configuration.
pub fn load_config(path: &Path) -> Result<BeamConfig> {
    if !path.exists() {
        tracing::warn!(
            "Config file not found at {}, using defaults",
            path.display()
        );
        return Ok(BeamConfig {
            server: ServerConfig::default(),
            video: VideoConfig::default(),
            audio: AudioConfig::default(),
            session: SessionConfig::default(),
        });
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: BeamConfig =
        toml::from_str(&contents).with_context(|| "Failed to parse config TOML")?;

    tracing::info!("Loaded config from {}", path.display());
    Ok(config)
}
