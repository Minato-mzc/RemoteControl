use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    #[serde(rename = "hello")]
    Hello {
        v: u32,
        c: String,
        nonce: String,
        #[serde(default)]
        client: ClientInfo,
    },
    #[serde(rename = "ping")]
    Ping {
        #[serde(default)]
        ts: u64,
    },

    // ---- M2: stream control ----

    #[serde(rename = "stream_request")]
    StreamRequest {
        #[serde(default = "default_codec")]
        codec: String,
        #[serde(default)]
        max_bitrate_kbps: Option<u32>,
        #[serde(default)]
        max_fps: Option<u32>,
        #[serde(default)]
        prefer_keyframe_interval_ms: Option<u32>,
    },

    #[serde(rename = "stream_stop")]
    StreamStop {
        #[serde(default)]
        stream_id: Option<String>,
    },

    #[serde(rename = "keyframe_request")]
    KeyframeRequest {
        #[serde(default)]
        stream_id: Option<String>,
    },

    // ---- M3: mouse input (client → server) ----

    /// Absolute mouse position; x/y normalized to [0.0, 1.0].
    #[serde(rename = "mouse_move")]
    MouseMove { x: f32, y: f32 },

    /// Mouse button press/release.
    #[serde(rename = "mouse_button")]
    MouseButton { button: MouseButton, down: bool },

    /// Mouse wheel delta in notches (positive = up/right, negative = down/left).
    #[serde(rename = "mouse_scroll")]
    MouseScroll {
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
    },

    // ---- M3.5: keyboard input ----

    /// Unicode text injection (one key event per scalar, KEYEVENTF_UNICODE).
    #[serde(rename = "key_text")]
    KeyText { text: String },

    /// Virtual-key down/up. `vk` is a Win32 VK_* code.
    #[serde(rename = "key_event")]
    KeyEvent { vk: u32, down: bool },

    // ---- M8: clipboard sync ----

    /// Push text from the phone to the PC clipboard.
    #[serde(rename = "clipboard_set")]
    ClipboardSet { text: String },

    /// Request the PC clipboard (server replies with `clipboard_text`).
    #[serde(rename = "clipboard_get")]
    ClipboardGet,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

fn default_codec() -> String {
    "h264".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub app_version: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ServerMsg {
    #[serde(rename = "welcome")]
    Welcome {
        session: String,
        server: ServerInfo,
        hmac: String,
    },
    #[serde(rename = "error")]
    Error { code: ErrorCode, msg: String },
    #[serde(rename = "pong")]
    Pong { ts: u64 },

    // ---- M2: stream control ----

    #[serde(rename = "stream_started")]
    StreamStarted {
        stream_id: String,
        codec: String,
        profile: String,
        level: String,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        keyframe_interval_frames: u32,
        pixel_format: String,
        started_at_unix_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<AudioMetadata>,
    },

    #[serde(rename = "stream_stopped")]
    StreamStopped {
        stream_id: String,
        reason: StreamStopReason,
        msg: String,
    },

    /// Reply to a `clipboard_get` with the current PC clipboard text.
    #[serde(rename = "clipboard_text")]
    ClipboardText { text: String },
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub os: String,
    pub version: String,
}

/// Audio sub-stream metadata embedded in [`ServerMsg::StreamStarted`] when the
/// server captures system audio alongside the screen.
#[derive(Debug, Serialize, Clone)]
pub struct AudioMetadata {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub frame_size_ms: u32,
    pub bitrate_kbps: u32,
    /// Base64 of Opus ID Header bytes (Android MediaCodec csd-0).
    pub csd_0_b64: String,
    /// Base64 of pre-skip nanoseconds as LE i64 (csd-1).
    pub csd_1_b64: String,
    /// Base64 of seek pre-roll nanoseconds as LE i64 (csd-2).
    pub csd_2_b64: String,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadPairingCode,
    CodeExpired,
    CodeUsed,
    VersionMismatch,
    Malformed,
    // M2
    StreamUnavailable,
    StreamAlreadyRunning,
    NotAuthenticated,
    // M3
    #[allow(dead_code)]
    InputUnavailable,
    // M3.5
    #[allow(dead_code)]
    KeyEventInvalid,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum StreamStopReason {
    ClientRequested,
    EncoderError,
    CaptureError,
    ServerShutdown,
}

// ============================================================================
// Binary video frame format (data plane, WebSocket Binary frame)
// ============================================================================
//
// See docs/PROTOCOL.md §"数据平面" for the wire layout. Constants here mirror
// the documented byte offsets so encode/decode stays in lock-step with the spec.

pub const FRAME_HEADER_LEN: usize = 12;

/// `frame_type` byte values
pub mod frame_type {
    pub const VIDEO: u8 = 0x01;
    #[allow(dead_code)]
    pub const AUDIO: u8 = 0x02; // reserved for M5
}

/// `flags` byte bits
pub mod frame_flags {
    /// IDR / keyframe — the receiver may resume from this point.
    pub const KEYFRAME: u8 = 1 << 0;
    /// Payload is prefixed with parameter sets (SPS/PPS), inline. Always set on H.264 IDR frames.
    pub const CONFIG: u8 = 1 << 1;
}

/// Build a video frame: 12-byte header + Annex-B payload, ready for `WebSocket::send(Binary)`.
pub fn build_video_frame(payload: &[u8], pts_us: u64, keyframe: bool, has_config: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(frame_type::VIDEO);
    let mut flags = 0u8;
    if keyframe {
        flags |= frame_flags::KEYFRAME;
    }
    if has_config {
        flags |= frame_flags::CONFIG;
    }
    out.push(flags);
    out.extend_from_slice(&[0u8, 0u8]); // reserved
    out.extend_from_slice(&pts_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Build an audio frame: 12-byte header (frame_type=AUDIO, flags=0) + Opus payload.
pub fn build_audio_frame(payload: &[u8], pts_us: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(frame_type::AUDIO);
    out.push(0); // flags
    out.extend_from_slice(&[0u8, 0u8]); // reserved
    out.extend_from_slice(&pts_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_video_frame_layout_matches_spec() {
        let payload = [0xAAu8, 0xBB, 0xCC];
        let frame = build_video_frame(&payload, 0x0102_0304_0506_0708, true, true);
        assert_eq!(frame.len(), FRAME_HEADER_LEN + payload.len());
        assert_eq!(frame[0], frame_type::VIDEO);
        assert_eq!(frame[1], frame_flags::KEYFRAME | frame_flags::CONFIG);
        assert_eq!(&frame[2..4], &[0, 0]);
        assert_eq!(&frame[4..12], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&frame[12..], &payload);
    }

    #[test]
    fn p_frame_has_no_flags() {
        let frame = build_video_frame(&[0u8; 8], 1234, false, false);
        assert_eq!(frame[1], 0);
    }
}
