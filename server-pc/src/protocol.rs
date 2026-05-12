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

    /// "I've paired with this server before, here's the long-lived token I
    /// got back then." Lets the phone reconnect without scanning a QR code
    /// every time the server restarts. The token is a 256-bit random value
    /// minted by the server on the first successful Hello and remembered
    /// in `trusted_devices.json`.
    #[serde(rename = "trusted_hello")]
    TrustedHello {
        v: u32,
        device_id: String,
        token: String,
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

    // ---- M6: file transfer (phone → PC v1) ----

    /// Announce a new file-transfer session. Server allocates an output
    /// path under the user's Downloads/RemoteControl folder, opens the
    /// file, and replies with `FileTransferAccepted`. Subsequent Binary
    /// frames with `frame_type=FILE` and the same `id` carry chunked
    /// content; the final chunk has the `LAST` flag set.
    #[serde(rename = "file_transfer_begin")]
    FileTransferBegin {
        id: u32,
        name: String,
        size: u64,
    },

    /// Voluntary cancel before reaching the LAST chunk. Server closes
    /// and unlinks the partial output.
    #[serde(rename = "file_transfer_abort")]
    FileTransferAbort {
        id: u32,
        #[serde(default)]
        reason: String,
    },

    // ---- M6 v2: file transfer (PC → phone). Phone is the receiver. ----

    /// Phone confirms it opened a destination file for the incoming
    /// transfer announced by `FileSendBegin`. PC may now start
    /// streaming chunks (FILE binary frames carrying the same `id`).
    /// `dest_path` is whatever the phone chose to display to the user —
    /// app-private external storage path, MediaStore URI string, etc.
    #[serde(rename = "file_send_accepted")]
    FileSendAccepted { id: u32, dest_path: String },

    /// Phone has written the last chunk to disk and finalized the file.
    /// Returns the same `dest_path` so the PC UI can confirm.
    #[serde(rename = "file_send_complete")]
    FileSendComplete { id: u32, dest_path: String },

    /// Phone aborted: out of space, IO error, user declined, transfer
    /// id unknown, etc. PC reports the reason and forgets this id.
    #[serde(rename = "file_send_failed")]
    FileSendFailed { id: u32, reason: String },
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
        /// Long-lived token the client should persist and use in future
        /// `trusted_hello` messages — saves the user from scanning the QR
        /// every time. Only present on a successful first-time Hello,
        /// not on a TrustedHello reconnect (the client already has it).
        #[serde(skip_serializing_if = "Option::is_none")]
        trust_token: Option<String>,
        /// Stable identifier the server assigned to this device. The
        /// client echoes it back in `trusted_hello.device_id` so the
        /// server can look up the right token in O(1).
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
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

    // ---- M6: file transfer ----

    /// Server accepted the transfer and opened the destination file.
    /// Phone can start streaming chunks.
    #[serde(rename = "file_transfer_accepted")]
    FileTransferAccepted { id: u32, dest_path: String },

    /// Final ack — all chunks received, file closed and renamed into
    /// place. `dest_path` is the on-disk path (useful for the phone UI
    /// to show "已保存到 ...").
    #[serde(rename = "file_transfer_complete")]
    FileTransferComplete { id: u32, dest_path: String },

    /// Anything went wrong — bad name, no space, IO error, premature
    /// disconnect, abort, etc. Phone shows the reason to the user.
    #[serde(rename = "file_transfer_failed")]
    FileTransferFailed { id: u32, reason: String },

    // ---- M6 v2: file transfer (PC → phone). Mirrors `FileTransfer*`
    // but with the directions inverted. PC is the sender. ----

    /// PC announces an incoming file. Phone allocates a destination
    /// (app-private external storage in v1 so we don't need a runtime
    /// storage permission), opens the file, and replies with
    /// `FileSendAccepted`. PC then streams FILE Binary frames carrying
    /// the same `id`; the last chunk has the `LAST` flag set.
    #[serde(rename = "file_send_begin")]
    FileSendBegin {
        id: u32,
        name: String,
        size: u64,
    },
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
    /// Reconnect path: device_id wasn't in `trusted_devices.json` (first
    /// time on a new server, or the file was wiped). Phone should fall
    /// back to QR pairing.
    UnknownDevice,
    /// Reconnect path: device_id known, but the supplied token didn't
    /// match. Either the token was rotated/revoked or it's a malicious
    /// client. Phone should clear its stored token and fall back to QR.
    BadTrustToken,
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
    /// M6 file-transfer chunk. Reuses the 12-byte header layout; the
    /// 8 bytes that hold `pts_us` for video are repurposed as
    /// `transfer_id (u32 LE) || chunk_seq (u32 LE)`.
    pub const FILE: u8 = 0x03;
}

/// `flags` byte bits
pub mod frame_flags {
    /// IDR / keyframe — the receiver may resume from this point.
    pub const KEYFRAME: u8 = 1 << 0;
    /// Payload is prefixed with parameter sets (SPS/PPS), inline. Always set on H.264 IDR frames.
    pub const CONFIG: u8 = 1 << 1;
    /// M6 file-transfer: marks the FINAL chunk in a transfer. After
    /// receiving a frame with this bit set, the server flushes + closes
    /// the destination file and emits `FileTransferComplete`.
    pub const LAST_CHUNK: u8 = 1 << 0;
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

/// Parse a binary frame's 12-byte header. Returns `(frame_type, flags,
/// pts_or_id_seq_bytes, payload_offset)`. Caller dispatches on
/// `frame_type` and re-interprets the 8-byte field accordingly.
pub fn peek_frame_header(buf: &[u8]) -> Option<(u8, u8)> {
    if buf.len() < FRAME_HEADER_LEN {
        return None;
    }
    Some((buf[0], buf[1]))
}

/// Build a FILE chunk frame for the PC → phone path (M6 v2). Mirror
/// of `parse_file_chunk`: the 8 PTS bytes carry `transfer_id (u32 LE)
/// || chunk_seq (u32 LE)`. `last_chunk` sets `frame_flags::LAST_CHUNK`
/// so the phone knows to finalize the destination file after writing
/// this payload.
pub fn build_file_chunk_frame(
    transfer_id: u32,
    chunk_seq: u32,
    last_chunk: bool,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(frame_type::FILE);
    out.push(if last_chunk { frame_flags::LAST_CHUNK } else { 0 });
    out.extend_from_slice(&[0u8, 0u8]); // reserved
    out.extend_from_slice(&transfer_id.to_le_bytes());
    out.extend_from_slice(&chunk_seq.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode a FILE chunk header (the 8 PTS bytes are split into
/// `transfer_id (u32 LE) || chunk_seq (u32 LE)`). Returns
/// `(transfer_id, chunk_seq, is_last, payload_slice)`.
pub fn parse_file_chunk(buf: &[u8]) -> Option<(u32, u32, bool, &[u8])> {
    if buf.len() < FRAME_HEADER_LEN {
        return None;
    }
    let flags = buf[1];
    let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let seq = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let is_last = (flags & frame_flags::LAST_CHUNK) != 0;
    Some((id, seq, is_last, &buf[FRAME_HEADER_LEN..]))
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
