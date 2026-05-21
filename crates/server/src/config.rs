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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-config-test-{}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            name,
        ))
    }

    #[test]
    fn load_config_returns_defaults_when_file_missing() {
        let path = unique_temp_path("missing.toml");
        assert!(!path.exists(), "Pre-condition: path must not exist");

        let config = load_config(&path).expect("Default config should load");
        // Spot-check that the returned struct has the default-shaped subobjects.
        // The exact defaults are owned by beam-protocol; we just verify the
        // call assembled a complete BeamConfig without panicking.
        let _ = config.server;
        let _ = config.video;
        let _ = config.audio;
        let _ = config.session;
    }

    #[test]
    fn load_config_parses_valid_toml() {
        let dir = unique_temp_path("valid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("beam.toml");
        // Minimal valid config: rely on defaults for everything not specified.
        // An empty TOML document deserialises into a BeamConfig where every
        // section uses its serde default, exercising the success branch end-to-end.
        std::fs::write(&path, "").expect("write temp config");

        let config = load_config(&path).expect("Empty TOML should yield defaults");
        let _ = config.server;
        let _ = config.video;
        let _ = config.audio;
        let _ = config.session;

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_errors_on_malformed_toml() {
        let dir = unique_temp_path("invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("beam.toml");
        std::fs::write(&path, "this = is = not = valid =").expect("write temp config");

        let err = load_config(&path).expect_err("Malformed TOML must fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("parse config TOML"),
            "Error chain should mention the parse stage, got: {chain}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_errors_on_unreadable_path() {
        // A directory pretending to be a config file: exists() == true so we
        // bypass the early return, but read_to_string fails. Exercises the
        // read-stage error branch.
        let dir = unique_temp_path("isdir");
        std::fs::create_dir_all(&dir).expect("create dir-as-file");
        assert!(dir.exists());

        let err = load_config(&dir).expect_err("Reading a directory must fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Failed to read config file"),
            "Error chain should mention the read stage, got: {chain}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
