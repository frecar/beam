use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Signaling messages between browser, server, and agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalingMessage {
    /// WebRTC SDP offer from browser
    Offer { sdp: String, session_id: Uuid },
    /// WebRTC SDP answer from agent
    Answer { sdp: String, session_id: Uuid },
    /// ICE candidate exchange
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
        session_id: Uuid,
    },
    /// Session created successfully
    SessionReady { session_id: Uuid },
    /// Error
    Error { message: String },
}

/// Input events sent over the WebRTC DataChannel (compact format).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum InputEvent {
    /// Key press/release: evdev code + down state
    #[serde(rename = "k")]
    Key {
        /// Linux evdev key code
        c: u16,
        /// true = pressed, false = released
        d: bool,
    },
    /// Mouse move: normalized coordinates (0.0 - 1.0)
    #[serde(rename = "m")]
    MouseMove { x: f64, y: f64 },
    /// Relative mouse move (pointer lock mode): raw pixel deltas
    #[serde(rename = "rm")]
    RelativeMouseMove { dx: f64, dy: f64 },
    /// Mouse button press/release
    #[serde(rename = "b")]
    Button {
        /// Button index (0=left, 1=middle, 2=right)
        b: u8,
        /// true = pressed, false = released
        d: bool,
    },
    /// Scroll event
    #[serde(rename = "s")]
    Scroll { dx: f64, dy: f64 },
    /// Clipboard text (CLIPBOARD selection)
    #[serde(rename = "c")]
    Clipboard { text: String },
    /// Clipboard text for X11 PRIMARY selection (middle-click paste)
    #[serde(rename = "cp")]
    ClipboardPrimary { text: String },
    /// Resolution change request
    #[serde(rename = "r")]
    Resize { w: u32, h: u32 },
    /// Keyboard layout hint (XKB layout name, e.g. "no", "us", "de")
    #[serde(rename = "l")]
    Layout { layout: String },
    /// Quality mode: "high" (LAN) or "low" (WAN)
    #[serde(rename = "q")]
    Quality { mode: String },
    /// Browser tab visibility state (true = visible, false = hidden/backgrounded)
    #[serde(rename = "vs")]
    VisibilityState { visible: bool },
    /// File transfer start: initiates a new file upload
    #[serde(rename = "fs")]
    FileStart { id: String, name: String, size: u64 },
    /// File transfer chunk: base64-encoded file data
    #[serde(rename = "fc")]
    FileChunk { id: String, data: String },
    /// File transfer done: signals upload is complete
    #[serde(rename = "fd")]
    FileDone { id: String },
    /// File download request: browser asks agent to send a file
    #[serde(rename = "fdr")]
    FileDownloadRequest { path: String },
}

/// Authentication request.
/// Password is redacted in Debug output to prevent accidental logging.
#[derive(Serialize, Deserialize)]
pub struct AuthRequest {
    pub username: String,
    pub password: String,
    /// Browser viewport width in CSS pixels (used to set initial display resolution).
    pub viewport_width: Option<u32>,
    /// Browser viewport height in CSS pixels.
    pub viewport_height: Option<u32>,
    /// Per-session idle timeout override in seconds. None = use global default.
    /// Must be in range 60..=86400 (1 minute to 24 hours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
}

impl std::fmt::Debug for AuthRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRequest")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Authentication response
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthResponse {
    pub token: String,
    pub session_id: Uuid,
    /// Short token for graceful session release via `navigator.sendBeacon()`
    /// on browser tab close. Separate from the JWT since sendBeacon cannot
    /// set Authorization headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_token: Option<String>,
    /// Effective idle timeout for this session in seconds (0 = disabled).
    /// Returned so the client can show accurate idle warnings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
}

/// ICE server configuration returned to clients for WebRTC setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServerInfo {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Session information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub username: String,
    pub display: u32,
    pub width: u32,
    pub height: u32,
    pub created_at: u64,
}

/// Internal message from server to agent process.
/// Uses adjacently tagged representation to avoid tag collision with nested SignalingMessage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "data", rename_all = "snake_case")]
pub enum AgentCommand {
    /// Forward a signaling message to the agent
    Signal(SignalingMessage),
    /// Shut down the agent
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signaling_offer_roundtrip() {
        let msg = SignalingMessage::Offer {
            sdp: "v=0\r\n...".to_string(),
            session_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"offer""#));
        let parsed: SignalingMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SignalingMessage::Offer { sdp, .. } => assert_eq!(sdp, "v=0\r\n..."),
            _ => panic!("Expected Offer"),
        }
    }

    #[test]
    fn signaling_answer_roundtrip() {
        let msg = SignalingMessage::Answer {
            sdp: "v=0\r\nanswer".to_string(),
            session_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"answer""#));
        let _: SignalingMessage = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn signaling_ice_candidate_snake_case() {
        let msg = SignalingMessage::IceCandidate {
            candidate: "candidate:1 1 UDP 2130706431 ...".to_string(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
            session_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        // Must be snake_case, NOT kebab-case
        assert!(json.contains(r#""type":"ice_candidate""#));
        assert!(!json.contains("ice-candidate"));

        let parsed: SignalingMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SignalingMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ..
            } => {
                assert!(candidate.starts_with("candidate:"));
                assert_eq!(sdp_mid, Some("0".to_string()));
                assert_eq!(sdp_mline_index, Some(0));
            }
            _ => panic!("Expected IceCandidate"),
        }
    }

    #[test]
    fn ice_candidate_from_browser_format() {
        // Simulate what the web client sends
        let browser_json = r#"{
            "type": "ice_candidate",
            "candidate": "candidate:1 1 UDP 2130706431 192.168.1.1 50000 typ host",
            "sdp_mid": "0",
            "sdp_mline_index": 0,
            "session_id": "00000000-0000-0000-0000-000000000000"
        }"#;
        let msg: SignalingMessage = serde_json::from_str(browser_json).unwrap();
        match msg {
            SignalingMessage::IceCandidate { candidate, .. } => {
                assert!(candidate.contains("candidate:1"));
            }
            _ => panic!("Expected IceCandidate"),
        }
    }

    #[test]
    fn input_event_compact_format() {
        let key = InputEvent::Key { c: 30, d: true };
        let json = serde_json::to_string(&key).unwrap();
        assert!(json.contains(r#""t":"k""#));
        assert!(json.contains(r#""c":30"#));
        assert!(json.contains(r#""d":true"#));

        let mouse = InputEvent::MouseMove { x: 0.5, y: 0.75 };
        let json = serde_json::to_string(&mouse).unwrap();
        assert!(json.contains(r#""t":"m""#));

        let scroll = InputEvent::Scroll { dx: 0.0, dy: -30.0 };
        let json = serde_json::to_string(&scroll).unwrap();
        assert!(json.contains(r#""t":"s""#));

        let clip = InputEvent::Clipboard {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&clip).unwrap();
        assert!(json.contains(r#""t":"c""#));

        let clip_primary = InputEvent::ClipboardPrimary {
            text: "primary".to_string(),
        };
        let json = serde_json::to_string(&clip_primary).unwrap();
        assert!(json.contains(r#""t":"cp""#));
        assert!(json.contains(r#""text":"primary""#));

        let resize = InputEvent::Resize { w: 1920, h: 1080 };
        let json = serde_json::to_string(&resize).unwrap();
        assert!(json.contains(r#""t":"r""#));

        let layout = InputEvent::Layout {
            layout: "no".to_string(),
        };
        let json = serde_json::to_string(&layout).unwrap();
        assert!(json.contains(r#""t":"l""#));
        assert!(json.contains(r#""layout":"no""#));

        let rel_mouse = InputEvent::RelativeMouseMove { dx: -3.5, dy: 1.2 };
        let json = serde_json::to_string(&rel_mouse).unwrap();
        assert!(json.contains(r#""t":"rm""#));
        assert!(json.contains(r#""dx""#));
        assert!(json.contains(r#""dy""#));

        let visibility = InputEvent::VisibilityState { visible: false };
        let json = serde_json::to_string(&visibility).unwrap();
        assert!(json.contains(r#""t":"vs""#));
        assert!(json.contains(r#""visible":false"#));

        let visibility_true = InputEvent::VisibilityState { visible: true };
        let json = serde_json::to_string(&visibility_true).unwrap();
        assert!(json.contains(r#""t":"vs""#));
        assert!(json.contains(r#""visible":true"#));

        // Verify deserialization from browser format
        let browser_vs: InputEvent = serde_json::from_str(r#"{"t":"vs","visible":false}"#).unwrap();
        match browser_vs {
            InputEvent::VisibilityState { visible } => assert!(!visible),
            _ => panic!("Expected VisibilityState"),
        }

        // File transfer events
        let fs = InputEvent::FileStart {
            id: "abc-123".to_string(),
            name: "test.txt".to_string(),
            size: 1024,
        };
        let json = serde_json::to_string(&fs).unwrap();
        assert!(json.contains(r#""t":"fs""#));
        assert!(json.contains(r#""id":"abc-123""#));
        assert!(json.contains(r#""name":"test.txt""#));
        assert!(json.contains(r#""size":1024"#));

        let fc = InputEvent::FileChunk {
            id: "abc-123".to_string(),
            data: "SGVsbG8=".to_string(),
        };
        let json = serde_json::to_string(&fc).unwrap();
        assert!(json.contains(r#""t":"fc""#));
        assert!(json.contains(r#""data":"SGVsbG8=""#));

        let fd = InputEvent::FileDone {
            id: "abc-123".to_string(),
        };
        let json = serde_json::to_string(&fd).unwrap();
        assert!(json.contains(r#""t":"fd""#));

        // Verify deserialization from browser format
        let browser_fs: InputEvent =
            serde_json::from_str(r#"{"t":"fs","id":"x","name":"f.txt","size":42}"#).unwrap();
        match browser_fs {
            InputEvent::FileStart { id, name, size } => {
                assert_eq!(id, "x");
                assert_eq!(name, "f.txt");
                assert_eq!(size, 42);
            }
            _ => panic!("Expected FileStart"),
        }

        // File download request
        let fdr = InputEvent::FileDownloadRequest {
            path: "/home/user/file.txt".to_string(),
        };
        let json = serde_json::to_string(&fdr).unwrap();
        assert!(json.contains(r#""t":"fdr""#));
        assert!(json.contains(r#""path":"/home/user/file.txt""#));

        let browser_fdr: InputEvent =
            serde_json::from_str(r#"{"t":"fdr","path":"/home/user/doc.pdf"}"#).unwrap();
        match browser_fdr {
            InputEvent::FileDownloadRequest { path } => {
                assert_eq!(path, "/home/user/doc.pdf");
            }
            _ => panic!("Expected FileDownloadRequest"),
        }
    }

    #[test]
    fn input_event_from_browser() {
        let browser_json = r#"{"t":"k","c":30,"d":true}"#;
        let event: InputEvent = serde_json::from_str(browser_json).unwrap();
        match event {
            InputEvent::Key { c, d } => {
                assert_eq!(c, 30);
                assert!(d);
            }
            _ => panic!("Expected Key"),
        }
    }

    #[test]
    fn agent_command_wraps_signal() {
        let offer = SignalingMessage::Offer {
            sdp: "test".to_string(),
            session_id: Uuid::nil(),
        };
        let cmd = AgentCommand::Signal(offer);
        let json = serde_json::to_string(&cmd).unwrap();
        // Uses adjacently tagged: {"cmd":"signal","data":{...SignalingMessage...}}
        assert!(json.contains(r#""cmd":"signal""#));
        assert!(json.contains(r#""data""#));
        assert!(json.contains(r#""sdp":"test""#));
        // The nested SignalingMessage retains its own "type" tag
        assert!(json.contains(r#""type":"offer""#));

        // Verify the agent can parse it back
        let parsed: AgentCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            AgentCommand::Signal(SignalingMessage::Offer { sdp, .. }) => {
                assert_eq!(sdp, "test");
            }
            _ => panic!("Expected Signal(Offer)"),
        }
    }

    #[test]
    fn agent_command_shutdown() {
        let cmd = AgentCommand::Shutdown;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""cmd":"shutdown""#));
        let parsed: AgentCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentCommand::Shutdown));
    }

    #[test]
    fn auth_request_password_redacted_in_debug() {
        let req = AuthRequest {
            username: "admin".to_string(),
            password: "super_secret".to_string(),
            viewport_width: None,
            viewport_height: None,
            idle_timeout: None,
        };
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("admin"));
        assert!(debug_str.contains("[REDACTED]"));
        assert!(!debug_str.contains("super_secret"));
    }

    #[test]
    fn auth_request_without_idle_timeout() {
        let json = r#"{"username":"user","password":"pass"}"#;
        let req: AuthRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.username, "user");
        assert!(req.idle_timeout.is_none());
    }

    #[test]
    fn auth_request_with_idle_timeout() {
        let json = r#"{"username":"user","password":"pass","idle_timeout":7200}"#;
        let req: AuthRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.idle_timeout, Some(7200));
    }

    #[test]
    fn auth_request_idle_timeout_skipped_when_none() {
        let req = AuthRequest {
            username: "user".to_string(),
            password: "pass".to_string(),
            viewport_width: None,
            viewport_height: None,
            idle_timeout: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("idle_timeout"));
    }

    #[test]
    fn auth_response_with_idle_timeout() {
        let resp = AuthResponse {
            token: "tok".to_string(),
            session_id: Uuid::nil(),
            release_token: None,
            idle_timeout: Some(3600),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""idle_timeout":3600"#));
    }

    #[test]
    fn auth_response_idle_timeout_skipped_when_none() {
        let resp = AuthResponse {
            token: "tok".to_string(),
            session_id: Uuid::nil(),
            release_token: None,
            idle_timeout: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("idle_timeout"));
    }

    #[test]
    fn config_defaults() {
        let config: crate::BeamConfig = toml::from_str("").unwrap();
        assert_eq!(config.server.port, 8443);
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.video.bitrate, 5000);
        assert_eq!(config.video.min_bitrate, 500);
        assert_eq!(config.video.max_bitrate, 20000);
        assert_eq!(config.video.framerate, 60);
        assert!(config.audio.enabled);
        assert_eq!(config.session.max_sessions, 8);
        assert_eq!(config.session.default_width, 1920);
        assert_eq!(config.session.default_height, 1080);
        // ICE defaults
        assert_eq!(config.ice.stun_urls.len(), 2);
        assert!(config.ice.turn_urls.is_empty());
        assert!(config.ice.turn_username.is_none());
    }
}
