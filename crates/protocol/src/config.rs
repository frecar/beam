use serde::{Deserialize, Serialize};

/// Top-level configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeamConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub ice: IceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address
    #[serde(default = "default_bind")]
    pub bind: String,
    /// HTTPS port
    #[serde(default = "default_port")]
    pub port: u16,
    /// Path to TLS certificate (auto-generated if absent)
    pub tls_cert: Option<String>,
    /// Path to TLS key (auto-generated if absent)
    pub tls_key: Option<String>,
    /// JWT secret (auto-generated if absent)
    pub jwt_secret: Option<String>,
    /// Path to web client static files
    #[serde(default = "default_web_root")]
    pub web_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Target bitrate in kbps
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
    /// Minimum bitrate in kbps (for adaptive bitrate)
    #[serde(default = "default_min_bitrate")]
    pub min_bitrate: u32,
    /// Maximum bitrate in kbps (for adaptive bitrate)
    #[serde(default = "default_max_bitrate")]
    pub max_bitrate: u32,
    /// Target framerate
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    /// Force a specific encoder: "nvh264enc", "vah264enc", "x264enc"
    pub encoder: Option<String>,
    /// Maximum width (0 = unlimited, default: 3840)
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    /// Maximum height (0 = unlimited, default: 2160)
    #[serde(default = "default_max_height")]
    pub max_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Enable audio streaming
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Opus bitrate in kbps
    #[serde(default = "default_audio_bitrate")]
    pub bitrate: u32,
}

/// ICE/TURN server configuration for WebRTC NAT traversal.
///
/// Without TURN, WebRTC fails behind symmetric NATs (~20% of enterprise networks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceConfig {
    /// STUN server URLs (default: Google's public STUN servers)
    #[serde(default = "default_stun_urls")]
    pub stun_urls: Vec<String>,
    /// TURN server URLs (e.g., "turn:turn.example.com:3478")
    #[serde(default)]
    pub turn_urls: Vec<String>,
    /// TURN username (for long-term credential mechanism)
    pub turn_username: Option<String>,
    /// TURN credential/password
    pub turn_credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Default width
    #[serde(default = "default_width")]
    pub default_width: u32,
    /// Default height
    #[serde(default = "default_height")]
    pub default_height: u32,
    /// Starting X display number
    #[serde(default = "default_display_start")]
    pub display_start: u32,
    /// Maximum concurrent sessions
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// Idle timeout in seconds (0 = disabled)
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            tls_cert: None,
            tls_key: None,
            jwt_secret: None,
            web_root: default_web_root(),
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            bitrate: default_bitrate(),
            min_bitrate: default_min_bitrate(),
            max_bitrate: default_max_bitrate(),
            framerate: default_framerate(),
            encoder: None,
            max_width: default_max_width(),
            max_height: default_max_height(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bitrate: default_audio_bitrate(),
        }
    }
}

impl Default for IceConfig {
    fn default() -> Self {
        Self {
            stun_urls: default_stun_urls(),
            turn_urls: Vec::new(),
            turn_username: None,
            turn_credential: None,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            default_width: default_width(),
            default_height: default_height(),
            display_start: default_display_start(),
            max_sessions: default_max_sessions(),
            idle_timeout: default_idle_timeout(),
        }
    }
}

fn default_web_root() -> String {
    "web/dist".to_string()
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8443
}
fn default_bitrate() -> u32 {
    5000
}
fn default_min_bitrate() -> u32 {
    500
}
fn default_max_bitrate() -> u32 {
    20000
}
fn default_framerate() -> u32 {
    60
}
fn default_max_width() -> u32 {
    3840 // 4K
}
fn default_max_height() -> u32 {
    2160 // 4K
}
fn default_true() -> bool {
    true
}
fn default_audio_bitrate() -> u32 {
    128
}
fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_display_start() -> u32 {
    10
}
fn default_max_sessions() -> u32 {
    8
}
fn default_idle_timeout() -> u64 {
    3600 // 1 hour
}
fn default_stun_urls() -> Vec<String> {
    vec![
        "stun:stun.l.google.com:19302".to_string(),
        "stun:stun1.l.google.com:19302".to_string(),
    ]
}
