use crate::encoder::EncoderType;
use anyhow::Context;
use beam_protocol::{InputEvent, SignalingMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::stats::StatsReport;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// Monotonically increasing counter so the video loop can detect peer swaps.
/// Each call to `create_peer` bumps this. The video loop compares its cached
/// generation against `snapshot_with_gen` to know when to reset state.
static PEER_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Configuration for ICE servers passed to the agent.
#[derive(Clone)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Parameters needed to create a new WebRTC peer.
/// Bundled so peer recreation on browser reconnect is a single call.
#[derive(Clone)]
pub struct PeerConfig {
    pub ice_servers: Vec<IceServerConfig>,
    pub encoder_type: EncoderType,
}

/// A swappable peer: consumers clone the inner Arc, the signaling handler
/// replaces it atomically when a new browser connects.
pub type SharedPeer = Arc<tokio::sync::RwLock<Arc<WebRTCPeer>>>;

pub struct WebRTCPeer {
    peer_connection: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    video_sender: Arc<RTCRtpSender>,
    data_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    /// Monotonic generation counter. Changes every time a new peer is created.
    /// The video/audio write loops compare this to detect peer swaps and reset
    /// their internal state (was_connected, waiting_for_idr, etc.).
    pub generation: u64,
}

impl WebRTCPeer {
    pub async fn new(
        ice_servers: Vec<IceServerConfig>,
        encoder_type: EncoderType,
    ) -> anyhow::Result<Self> {
        let mut media_engine = MediaEngine::default();

        // Register ONLY H.264 + Opus. Do NOT use register_default_codecs()
        // which includes VP8/VP9/AV1 - the agent only sends H.264, so including
        // other codecs causes Chrome to sometimes negotiate VP8 and get 0x0 video.
        //
        // CRITICAL: Only register the profile that matches the encoder output.
        // nvh264enc outputs Main profile; VA-API/x264 output Constrained Baseline.
        // If the SDP profile doesn't match the actual H.264 bitstream, Chrome's
        // decoder may refuse to decode (black screen).
        let h264_feedback = vec![
            RTCPFeedback {
                typ: "goog-remb".into(),
                parameter: "".into(),
            },
            RTCPFeedback {
                typ: "ccm".into(),
                parameter: "fir".into(),
            },
            RTCPFeedback {
                typ: "nack".into(),
                parameter: "".into(),
            },
            RTCPFeedback {
                typ: "nack".into(),
                parameter: "pli".into(),
            },
            RTCPFeedback {
                typ: "transport-cc".into(),
                parameter: "".into(),
            },
        ];

        // Register H.264 profiles matching encoder output.
        // nvh264enc SPS header is 67 4d 40 28 = Main profile (0x4d),
        // constraint_set1_flag=1 (0x40), Level 4.0 (0x28).
        // We register 42e01f (Constrained Baseline) as well because Chrome
        // always offers it and can decode Main profile data regardless of SDP.
        // webrtc-rs fmtp matching compares first 2 bytes of profile-level-id,
        // so we need to match what Chrome offers exactly.
        let h264_fmtp = match encoder_type {
            EncoderType::Nvidia => {
                info!("Registering H.264 for NVIDIA encoder (42e01f + 4d001f)");
                // Register Constrained Baseline first (Chrome always offers this)
                media_engine.register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: MIME_TYPE_H264.to_string(),
                            clock_rate: 90000,
                            channels: 0,
                            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string(),
                            rtcp_feedback: h264_feedback.clone(),
                        },
                        payload_type: 125,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )?;
                // Also register Main profile for exact match if Chrome offers it
                media_engine.register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: MIME_TYPE_H264.to_string(),
                            clock_rate: 90000,
                            channels: 0,
                            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f".to_string(),
                            rtcp_feedback: h264_feedback,
                        },
                        payload_type: 102,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )?;
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            }
            _ => {
                info!(
                    "Registering H.264 Constrained Baseline profile for {:?} encoder",
                    encoder_type
                );
                media_engine.register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: MIME_TYPE_H264.to_string(),
                            clock_rate: 90000,
                            channels: 0,
                            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string(),
                            rtcp_feedback: h264_feedback,
                        },
                        payload_type: 125,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )?;
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            }
        };

        // Opus audio
        media_engine.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_string(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let rtc_ice_servers: Vec<RTCIceServer> = if ice_servers.is_empty() {
            vec![RTCIceServer {
                urls: vec![
                    "stun:stun.l.google.com:19302".to_string(),
                    "stun:stun1.l.google.com:19302".to_string(),
                ],
                ..Default::default()
            }]
        } else {
            ice_servers
                .into_iter()
                .map(|s| RTCIceServer {
                    urls: s.urls,
                    username: s.username.unwrap_or_default(),
                    credential: s.credential.unwrap_or_default(),
                })
                .collect()
        };

        let config = RTCConfiguration {
            ice_servers: rtc_ice_servers,
            ..Default::default()
        };

        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        // Create H.264 video track with fmtp matching the registered codec.
        // Without fmtp, the track may bind to the wrong codec variant
        // (e.g., packetization-mode=0 instead of 1).
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_string(),
                clock_rate: 90000,
                sdp_fmtp_line: h264_fmtp.to_string(),
                ..Default::default()
            },
            "video".to_string(),
            "beam".to_string(),
        ));

        // Create Opus audio track
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 2,
                ..Default::default()
            },
            "audio".to_string(),
            "beam".to_string(),
        ));

        // Add tracks to peer connection
        let video_sender = peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("Failed to add video track")?;

        peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("Failed to add audio track")?;

        let data_channel = Arc::new(Mutex::new(None));

        // Log connection state changes.
        // Do NOT close the peer connection on failure - a new SDP offer from a
        // reconnecting browser can restart ICE and recover the connection.
        // Closing the peer makes it permanently unusable, forcing a full re-login.
        peer_connection.on_peer_connection_state_change(Box::new(move |state| {
            match state {
                RTCPeerConnectionState::Failed => {
                    warn!("Peer connection failed (will recover on next browser offer)");
                }
                RTCPeerConnectionState::Disconnected => {
                    warn!("Peer connection disconnected (ICE reconnecting)");
                }
                _ => {
                    info!(?state, "Peer connection state changed");
                }
            }
            Box::pin(async {})
        }));

        let generation = PEER_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        info!(generation, "WebRTC peer connection created");

        Ok(Self {
            peer_connection,
            video_track,
            audio_track,
            video_sender,
            data_channel,
            generation,
        })
    }

    /// Start reading RTCP packets from the video sender to handle PLI/FIR
    /// keyframe requests from the browser. Without this, packet loss causes
    /// up to 1 second of video corruption (until the next periodic IDR).
    pub fn start_rtcp_reader(&self, on_keyframe_request: impl Fn() + Send + Sync + 'static) {
        let sender = Arc::clone(&self.video_sender);
        tokio::spawn(async move {
            while let Ok((packets, _)) = sender.read_rtcp().await {
                for pkt in &packets {
                    let pkt_any = pkt.as_any();
                    if pkt_any.is::<rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                        || pkt_any.is::<rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                    {
                        info!("Received PLI/FIR keyframe request from browser");
                        on_keyframe_request();
                    }
                }
            }
        });
    }

    pub async fn handle_offer(&self, sdp: &str) -> anyhow::Result<String> {
        // Log SDP at debug level (verbose, only needed for codec negotiation issues)
        debug!("=== OFFER SDP START ===");
        for line in sdp.lines() {
            if line.starts_with("m=")
                || line.starts_with("a=rtpmap:")
                || line.starts_with("a=fmtp:")
                || line.starts_with("a=group:")
                || line.starts_with("a=mid:")
                || line.starts_with("a=ssrc:")
            {
                debug!(sdp_line = line, "Offer SDP");
            }
        }
        debug!("=== OFFER SDP END ===");

        let offer =
            RTCSessionDescription::offer(sdp.to_string()).context("Failed to parse SDP offer")?;

        self.peer_connection
            .set_remote_description(offer)
            .await
            .context("Failed to set remote description")?;

        let answer = self
            .peer_connection
            .create_answer(None)
            .await
            .context("Failed to create answer")?;

        self.peer_connection
            .set_local_description(answer.clone())
            .await
            .context("Failed to set local description")?;

        // Log SDP at debug level
        debug!("=== ANSWER SDP START ===");
        for line in answer.sdp.lines() {
            if line.starts_with("m=")
                || line.starts_with("a=rtpmap:")
                || line.starts_with("a=fmtp:")
                || line.starts_with("a=group:")
                || line.starts_with("a=mid:")
                || line.starts_with("a=ssrc:")
                || line.starts_with("a=bundle")
            {
                debug!(sdp_line = line, "Answer SDP");
            }
        }
        debug!("=== ANSWER SDP END ===");
        Ok(answer.sdp)
    }

    pub async fn add_ice_candidate(
        &self,
        candidate: &str,
        sdp_mid: Option<&str>,
        sdp_mline_index: Option<u16>,
    ) -> anyhow::Result<()> {
        let init = RTCIceCandidateInit {
            candidate: candidate.to_string(),
            sdp_mid: sdp_mid.map(|s| s.to_string()),
            sdp_mline_index,
            ..Default::default()
        };

        self.peer_connection
            .add_ice_candidate(init)
            .await
            .context("Failed to add ICE candidate")?;

        info!(candidate, ?sdp_mid, ?sdp_mline_index, "ICE candidate added");
        Ok(())
    }

    pub async fn write_video_sample(&self, data: Vec<u8>, duration_ns: u64) -> anyhow::Result<()> {
        self.video_track
            .write_sample(&webrtc::media::Sample {
                data: bytes::Bytes::from(data),
                duration: Duration::from_nanos(duration_ns),
                ..Default::default()
            })
            .await
            .context("Failed to write video sample")?;

        Ok(())
    }

    pub async fn write_audio_sample(&self, data: &[u8], duration_ns: u64) -> anyhow::Result<()> {
        self.audio_track
            .write_sample(&webrtc::media::Sample {
                data: bytes::Bytes::copy_from_slice(data),
                duration: Duration::from_nanos(duration_ns),
                ..Default::default()
            })
            .await
            .context("Failed to write audio sample")?;

        Ok(())
    }

    pub fn on_ice_candidate(
        &self,
        callback: impl Fn(String, Option<String>, Option<u16>) + Send + Sync + 'static,
    ) {
        let callback = Arc::new(callback);
        self.peer_connection
            .on_ice_candidate(Box::new(move |candidate| {
                if let Some(c) = candidate {
                    match c.to_json() {
                        Ok(json) => {
                            let cb = Arc::clone(&callback);
                            cb(json.candidate, json.sdp_mid, json.sdp_mline_index);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to serialize ICE candidate: {e}");
                        }
                    }
                }
                Box::pin(async {})
            }));
    }

    pub fn on_input_event(&self, callback: impl Fn(InputEvent) + Send + Sync + 'static) {
        let callback = Arc::new(callback);
        let dc_storage = Arc::clone(&self.data_channel);

        self.peer_connection.on_data_channel(Box::new(move |dc| {
            let callback = Arc::clone(&callback);
            let dc_storage = Arc::clone(&dc_storage);

            Box::pin(async move {
                if dc.label() == "input" {
                    info!("Input data channel opened");
                    {
                        let mut storage = dc_storage.lock().await;
                        *storage = Some(Arc::clone(&dc));
                    }

                    let cb = Arc::clone(&callback);
                    dc.on_message(Box::new(move |msg| {
                        let cb = Arc::clone(&cb);
                        Box::pin(async move {
                            match serde_json::from_slice::<InputEvent>(&msg.data) {
                                Ok(event) => cb(event),
                                Err(e) => {
                                    warn!("Invalid input event: {e}");
                                }
                            }
                        })
                    }));
                }
            })
        }));
    }

    /// Send a text message to the browser via the data channel.
    pub async fn send_data_channel_message(&self, msg: &str) -> anyhow::Result<()> {
        let dc = self.data_channel.lock().await;
        if let Some(ref dc) = *dc {
            dc.send_text(msg.to_string())
                .await
                .context("Failed to send data channel message")?;
        }
        Ok(())
    }

    /// Get the WebRTC stats report for adaptive bitrate decisions.
    pub async fn get_stats(&self) -> StatsReport {
        self.peer_connection.get_stats().await
    }

    /// Check if the peer connection is currently connected.
    pub fn is_connected(&self) -> bool {
        self.peer_connection.connection_state() == RTCPeerConnectionState::Connected
    }

    /// Return outbound video packets_sent from WebRTC stats.
    /// Used by the video loop to detect the silent-drop bug where
    /// write_sample returns Ok but the RTP pipeline is broken.
    pub async fn video_packets_sent(&self) -> u64 {
        let stats = self.peer_connection.get_stats().await;
        for (_key, stat) in stats.reports.iter() {
            use webrtc::stats::StatsReportType;
            if let StatsReportType::OutboundRTP(rtp) = stat
                && rtp.kind == "video"
            {
                return rtp.packets_sent;
            }
        }
        0
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.peer_connection
            .close()
            .await
            .context("Failed to close peer connection")?;
        info!("Peer connection closed");
        Ok(())
    }
}

/// Create a new SharedPeer with all callbacks wired up.
/// Called once at startup and again on every new SDP Offer (browser reconnect).
pub async fn create_peer(
    config: &PeerConfig,
    signal_tx: &mpsc::Sender<SignalingMessage>,
    session_id: Uuid,
    force_keyframe: &Arc<AtomicBool>,
    input_callback: Arc<dyn Fn(InputEvent) + Send + Sync>,
) -> anyhow::Result<Arc<WebRTCPeer>> {
    let peer = Arc::new(
        WebRTCPeer::new(config.ice_servers.clone(), config.encoder_type)
            .await
            .context("Failed to create WebRTC peer")?,
    );

    // ICE candidate callback → signaling channel
    let ice_tx = signal_tx.clone();
    let sid = session_id;
    peer.on_ice_candidate(move |candidate, sdp_mid, sdp_mline_index| {
        let msg = SignalingMessage::IceCandidate {
            candidate,
            sdp_mid,
            sdp_mline_index,
            session_id: sid,
        };
        let _ = ice_tx.try_send(msg);
    });

    // RTCP PLI/FIR → force keyframe
    let kf = Arc::clone(force_keyframe);
    peer.start_rtcp_reader(move || {
        kf.store(true, Ordering::Relaxed);
    });

    // Input events from data channel
    let cb = Arc::clone(&input_callback);
    peer.on_input_event(move |event| {
        cb(event);
    });

    info!("New WebRTC peer created with all callbacks");
    Ok(peer)
}

/// Read the current peer from SharedPeer (cheap: clone Arc, release lock).
pub async fn snapshot(shared: &SharedPeer) -> Arc<WebRTCPeer> {
    Arc::clone(&*shared.read().await)
}

/// Snapshot the current peer and its generation in one read-lock acquisition.
pub async fn snapshot_with_gen(shared: &SharedPeer) -> (Arc<WebRTCPeer>, u64) {
    let peer = snapshot(shared).await;
    let generation = peer.generation;
    (peer, generation)
}
