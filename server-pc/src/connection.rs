//! Per-peer message loop, abstracted away from the underlying transport.
//!
//! Used by both the LAN WebSocket listener (`ws_server`) and the relay
//! tunnel multiplexer (`relay_client`). Each peer (one phone) is handled
//! by [`run_connection`]; both sides plug into the loop via two `mpsc`
//! channels so the state-machine code in here doesn't care whether the
//! bytes came from a real `WebSocketStream<TcpStream>` or from a
//! `TunnelFrame::Data` over the relay's host WS.
//!
//! This used to live inline in `ws_server::handle_connection`. The
//! split happened when we added cross-network relay support — keeping
//! the protocol code path-agnostic was the cleanest way to avoid two
//! parallel implementations of the same handshake / stream-control /
//! input-dispatch logic.

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::{Config, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION, SERVER_VERSION};
#[cfg(windows)]
use crate::input;
use crate::pairing::{PairingStore, VerifyResult};
use crate::protocol::{ClientMsg, ErrorCode, ServerInfo, ServerMsg, StreamStopReason};
use crate::stream::{start_stream, RequestedCodec, StreamHandle, StreamRequestParams};
use crate::trusted_devices::{TrustedDevicesStore, VerifyOutcome};

type HmacSha256 = Hmac<Sha256>;

/// Outbound channel: anything we send to the peer goes through this.
/// Closing the receiver (e.g. peer disconnected) makes our `send` error
/// and we tear down.
pub type OutboundTx = mpsc::UnboundedSender<Message>;
/// Inbound channel: every WebSocket frame (text or binary) the peer sent
/// us shows up here. Sender side is closed by the transport pump when
/// the connection drops; we then exit naturally.
pub type InboundRx = mpsc::UnboundedReceiver<Message>;

/// Drive one peer connection from authenticated handshake to disconnect.
///
/// Caller owns the transport — they pumped `inbox` from the wire and they
/// drain `outbox` onto the wire. We only care about the protocol.
///
/// `peer_label` is opaque (e.g. `"192.168.31.230:42068"` for LAN or
/// `"relay/abc123"` for tunneled phones). Used only in log lines.
pub async fn run_connection(
    peer_label: String,
    mut inbox: InboundRx,
    outbox: OutboundTx,
    pairing: Arc<PairingStore>,
    trusted: Arc<TrustedDevicesStore>,
    cfg: Arc<Config>,
) -> Result<()> {
    info!("peer connected: {peer_label}");

    let mut authenticated = false;
    let mut active_stream: Option<StreamHandle> = None;
    // M6 file transfer state. Keyed by `transfer_id` so multiple in-flight
    // uploads are independent (in practice the phone sends one at a time
    // but the protocol doesn't forbid more). Entries removed on
    // last-chunk, abort, or peer disconnect.
    let mut file_transfers: std::collections::HashMap<u32, FileTransferState> =
        std::collections::HashMap::new();

    loop {
        // Optional packet-receive future. Built fresh each iteration so the
        // borrow on `active_stream` doesn't outlive the select.
        let next_pkt = async {
            match active_stream.as_mut() {
                Some(s) => s.packets.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            msg = inbox.recv() => {
                let Some(msg) = msg else { break }; // transport gone
                match msg {
                    Message::Text(text) => {
                        match serde_json::from_str::<ClientMsg>(text.as_str()) {
                            Err(e) => {
                                warn!("malformed from {peer_label}: {e}");
                                send_error(&outbox, ErrorCode::Malformed, "cannot parse message");
                                break;
                            }
                            Ok(parsed) => match parsed {
                                // M6 file-transfer messages need async
                                // file IO so we handle them inline here
                                // rather than threading `&mut file_transfers`
                                // and async-ifying the whole
                                // `handle_client_msg` switch. Anything
                                // else falls through to the existing
                                // synchronous dispatch.
                                ClientMsg::FileTransferBegin { id, name, size } => {
                                    if !authenticated {
                                        send(
                                            &outbox,
                                            ServerMsg::FileTransferFailed {
                                                id,
                                                reason: "not authenticated".into(),
                                            },
                                        );
                                        continue;
                                    }
                                    handle_file_begin(
                                        id,
                                        &name,
                                        size,
                                        &peer_label,
                                        &mut file_transfers,
                                        &outbox,
                                    )
                                    .await;
                                }
                                ClientMsg::FileTransferAbort { id, reason } => {
                                    handle_file_abort(
                                        id,
                                        &reason,
                                        &peer_label,
                                        &mut file_transfers,
                                    )
                                    .await;
                                }
                                other => {
                                    let cont = handle_client_msg(
                                        other,
                                        &peer_label,
                                        &pairing,
                                        &trusted,
                                        &cfg,
                                        &mut authenticated,
                                        &mut active_stream,
                                        &outbox,
                                    );
                                    if !cont { break; }
                                }
                            },
                        }
                    }
                    Message::Binary(bytes) => {
                        // M6: phone→PC file chunks arrive as Binary
                        // frames with `frame_type=FILE`. Other frame
                        // types from the phone side don't exist yet
                        // (video/audio are server→phone only), so
                        // anything that isn't FILE is dropped with a
                        // log line. We also gate this on
                        // `authenticated` so an unauthed peer can't
                        // dump random bytes onto the user's disk.
                        if !authenticated {
                            warn!("{peer_label}: binary before auth — dropping");
                            continue;
                        }
                        if let Some((t, _flags)) = crate::protocol::peek_frame_header(&bytes) {
                            if t == crate::protocol::frame_type::FILE {
                                handle_file_chunk(
                                    &bytes,
                                    &peer_label,
                                    &mut file_transfers,
                                    &outbox,
                                )
                                .await;
                            } else {
                                warn!(
                                    "{peer_label}: unexpected client binary frame type {t}"
                                );
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(p) => {
                        // tokio-tungstenite auto-pongs by default for the LAN path,
                        // but we still see Pings if the transport doesn't. The
                        // relay tunnel never sends Ping, so this is a no-op there.
                        let _ = outbox.send(Message::Pong(p));
                    }
                    _ => {}
                }
            }

            maybe_pkt = next_pkt => {
                let Some(bin) = maybe_pkt else {
                    // Stream worker exited on its own (encoder error / capture loss).
                    if let Some(handle) = active_stream.take() {
                        let stop_msg = ServerMsg::StreamStopped {
                            stream_id: handle.stream_id.clone(),
                            reason: StreamStopReason::EncoderError,
                            msg: "stream worker exited".to_string(),
                        };
                        let _ = send(&outbox, stop_msg);
                    }
                    continue;
                };
                if outbox.send(Message::Binary(bin.into())).is_err() {
                    break;
                }
            }
        }
    }

    if let Some(handle) = active_stream.take() {
        handle.stop();
    }
    // M6: any in-flight uploads get their partial files unlinked so the
    // user doesn't end up with a half-written file sitting in Downloads.
    for (id, state) in file_transfers.drain() {
        drop(state.file);
        if let Err(e) = tokio::fs::remove_file(&state.dest_path).await {
            warn!(
                "{peer_label}: cleanup of partial upload {id} ({}) failed: {e}",
                state.dest_path.display()
            );
        }
    }
    info!("peer disconnected: {peer_label} (authenticated={authenticated})");
    Ok(())
}

/// Returns false → caller should drop the connection.
fn handle_client_msg(
    msg: ClientMsg,
    peer_label: &str,
    pairing: &PairingStore,
    trusted: &TrustedDevicesStore,
    cfg: &Config,
    authenticated: &mut bool,
    active_stream: &mut Option<StreamHandle>,
    outbox: &OutboundTx,
) -> bool {
    match msg {
        ClientMsg::Hello { v, c, nonce, client } => {
            if !(MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&v) {
                send_error(
                    outbox,
                    ErrorCode::VersionMismatch,
                    &format!(
                        "server accepts v={MIN_SUPPORTED_VERSION}..={PROTOCOL_VERSION}, got v={v}"
                    ),
                );
                return false;
            }
            match pairing.verify_and_consume(&c) {
                VerifyResult::Ok => {
                    let nonce_bytes = match URL_SAFE_NO_PAD.decode(nonce.as_bytes()) {
                        Ok(b) => b,
                        Err(_) => {
                            send_error(outbox, ErrorCode::Malformed, "nonce not base64url");
                            return false;
                        }
                    };
                    let key = pairing.key();
                    let mut mac = HmacSha256::new_from_slice(&key)
                        .expect("HMAC accepts any key length");
                    mac.update(&nonce_bytes);
                    let hmac_hex = hex_encode(&mac.finalize().into_bytes());
                    let session = uuid::Uuid::new_v4().to_string();
                    // Mint a long-lived trust token so the phone can skip the
                    // QR scan on later launches. Failure here is non-fatal —
                    // the phone falls back to QR pairing.
                    let (trust_token, device_id) = match trusted.mint(client.name.clone()) {
                        Ok((dev_id, tok)) => {
                            info!(
                                "minted trust token  device_id={dev_id}  device_name={:?}",
                                client.name
                            );
                            (Some(tok), Some(dev_id))
                        }
                        Err(e) => {
                            warn!("trusted_devices.mint failed: {e:#}");
                            (None, None)
                        }
                    };
                    info!(
                        "handshake OK  peer={peer_label} session={session} client={:?}",
                        client
                    );
                    send(
                        outbox,
                        ServerMsg::Welcome {
                            session,
                            server: ServerInfo {
                                name: cfg.server_name.clone(),
                                os: cfg.os.clone(),
                                version: SERVER_VERSION.to_string(),
                            },
                            hmac: hmac_hex,
                            trust_token,
                            device_id,
                        },
                    );
                    *authenticated = true;
                }
                VerifyResult::BadCode => {
                    send_error(outbox, ErrorCode::BadPairingCode, "wrong code");
                    return false;
                }
                VerifyResult::Expired => {
                    send_error(outbox, ErrorCode::CodeExpired, "code expired");
                    return false;
                }
                VerifyResult::Used => {
                    send_error(outbox, ErrorCode::CodeUsed, "code already used");
                    return false;
                }
            }
        }

        ClientMsg::TrustedHello {
            v,
            device_id,
            token,
            client,
        } => {
            if !(MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&v) {
                send_error(
                    outbox,
                    ErrorCode::VersionMismatch,
                    &format!(
                        "server accepts v={MIN_SUPPORTED_VERSION}..={PROTOCOL_VERSION}, got v={v}"
                    ),
                );
                return false;
            }
            match trusted.verify(&device_id, &token) {
                Ok(VerifyOutcome::Ok { device_name }) => {
                    let session = uuid::Uuid::new_v4().to_string();
                    info!(
                        "trusted reconnect OK  peer={peer_label} session={session}  device={device_name:?} (id={device_id}) client={:?}",
                        client
                    );
                    send(
                        outbox,
                        ServerMsg::Welcome {
                            session,
                            server: ServerInfo {
                                name: cfg.server_name.clone(),
                                os: cfg.os.clone(),
                                version: SERVER_VERSION.to_string(),
                            },
                            // No HMAC challenge for trusted reconnect — the
                            // token itself is the authenticator.
                            hmac: String::new(),
                            trust_token: None,
                            device_id: None,
                        },
                    );
                    *authenticated = true;
                }
                Ok(VerifyOutcome::UnknownDevice) => {
                    info!(
                        "trusted reconnect rejected (unknown device_id={device_id}) peer={peer_label}"
                    );
                    send_error(
                        outbox,
                        ErrorCode::UnknownDevice,
                        "device not trusted; please re-pair via QR",
                    );
                    return false;
                }
                Ok(VerifyOutcome::BadToken) => {
                    warn!(
                        "trusted reconnect rejected (bad token for device_id={device_id}) peer={peer_label}"
                    );
                    send_error(outbox, ErrorCode::BadTrustToken, "trust token mismatch");
                    return false;
                }
                Err(e) => {
                    warn!("trusted_devices.verify error: {e:#}");
                    send_error(outbox, ErrorCode::Malformed, "internal trust check failed");
                    return false;
                }
            }
        }

        ClientMsg::Ping { ts } => {
            if !*authenticated {
                send_error(outbox, ErrorCode::NotAuthenticated, "hello first");
                return false;
            }
            send(outbox, ServerMsg::Pong { ts });
        }

        ClientMsg::StreamRequest {
            codec,
            max_bitrate_kbps,
            max_fps,
            prefer_keyframe_interval_ms,
        } => {
            if !*authenticated {
                send_error(outbox, ErrorCode::NotAuthenticated, "hello first");
                return false;
            }
            let requested_codec = match codec.to_ascii_lowercase().as_str() {
                "h264" => RequestedCodec::H264,
                "hevc" | "h265" => RequestedCodec::Hevc,
                _ => {
                    send_error(
                        outbox,
                        ErrorCode::StreamUnavailable,
                        &format!("only h264/hevc supported, got {codec}"),
                    );
                    return true;
                }
            };
            if active_stream.is_some() {
                send_error(
                    outbox,
                    ErrorCode::StreamAlreadyRunning,
                    "stop the current stream first",
                );
                return true;
            }
            // Relay-mode bandwidth budget. The 30 Mbps default is fine on
            // the LAN path, but the relay tunnel adds two extra hops
            // (PC→VPS, VPS→phone) and the *VPS's outbound* is generally
            // the tightest link in the chain — Tencent Lighthouse plans
            // sell 3-5 Mbps outbound for the entry tier, which is also
            // where most users will land. Clamp to 3 Mbps with a 2 s
            // GOP, leaving ~25 % headroom over a 4 Mbps VPS for ACKs
            // and IDR bursts. Going *over* the VPS's outbound rate
            // causes the relay's kernel TCP send buffer to fill, our
            // bounded per-phone queue starts dropping P-frames at
            // insertion time, and the phone's H.264 decoder loses
            // reference until the next IDR — visible as smearing /
            // tearing / blocky artifacts for up to one GOP. Keep the
            // PC's encoder under the VPS ceiling and the artifact
            // budget collapses to "occasional dropped frame".
            // (Earlier 500 kbps + 5 s GOP was a workaround for the HK
            // cross-border link that frequently dipped under 1 Mbps;
            // not needed once we moved relay onto a Mainland VPS.)
            let on_relay = peer_label.starts_with("relay/");
            let max_bitrate_kbps = if on_relay {
                Some(max_bitrate_kbps.unwrap_or(30_000).min(3_000))
            } else {
                max_bitrate_kbps
            };
            let prefer_keyframe_interval_ms = if on_relay {
                Some(prefer_keyframe_interval_ms.unwrap_or(1_000).max(2_000))
            } else {
                prefer_keyframe_interval_ms
            };
            let params = StreamRequestParams {
                codec: requested_codec,
                max_bitrate_kbps,
                max_fps,
                keyframe_interval_ms: prefer_keyframe_interval_ms,
            };
            match start_stream(params) {
                Ok(handle) => {
                    let started = ServerMsg::StreamStarted {
                        stream_id: handle.stream_id.clone(),
                        codec: handle.codec_wire_name.to_string(),
                        profile: handle.profile.as_str().to_string(),
                        level: "4.2".to_string(),
                        width: handle.width,
                        height: handle.height,
                        fps: handle.fps,
                        bitrate_kbps: handle.bitrate_kbps,
                        keyframe_interval_frames: handle.keyframe_interval_frames,
                        pixel_format: "yuv420p".to_string(),
                        started_at_unix_ms: handle.started_at_unix_ms,
                        audio: handle.audio_metadata.clone(),
                    };
                    send(outbox, started);
                    *active_stream = Some(handle);
                }
                Err(e) => {
                    warn!("start_stream failed: {e:#}");
                    send_error(
                        outbox,
                        ErrorCode::StreamUnavailable,
                        &format!("could not start stream: {e}"),
                    );
                }
            }
        }

        ClientMsg::StreamStop { stream_id } => {
            if let Some(handle) = active_stream.take() {
                let id = handle.stream_id.clone();
                if matches!(stream_id, Some(ref s) if s != &id) {
                    // Mismatched id — still close out the active stream.
                }
                drop(handle);
                send(
                    outbox,
                    ServerMsg::StreamStopped {
                        stream_id: id,
                        reason: StreamStopReason::ClientRequested,
                        msg: String::new(),
                    },
                );
            }
        }

        ClientMsg::KeyframeRequest { stream_id: _ } => {
            if let Some(s) = active_stream.as_ref() {
                s.force_keyframe();
            }
        }

        // ---- M3 mouse / M3.5 keyboard / M8 clipboard ----
        // All of these are pure SendInput / clipboard side-effects with no
        // reply; on non-Windows they're stubbed because the input/clipboard
        // modules are gated behind cfg(windows).
        #[cfg(windows)]
        ClientMsg::MouseMove { x, y } => {
            let _ = input::move_to(x, y);
        }
        #[cfg(windows)]
        ClientMsg::MouseButton { button, down } => {
            info!("mouse_button {:?} down={}", button, down);
            let _ = input::button(button, down);
        }
        #[cfg(windows)]
        ClientMsg::MouseScroll { dx, dy } => {
            let _ = input::scroll(dx, dy);
        }
        #[cfg(windows)]
        ClientMsg::KeyText { text } => {
            let _ = input::type_unicode(&text);
        }
        #[cfg(windows)]
        ClientMsg::KeyEvent { vk, down } => {
            let _ = input::vkey(vk, down);
        }
        #[cfg(windows)]
        ClientMsg::ClipboardSet { text } => {
            let _ = crate::clipboard::write_text(&text);
        }
        #[cfg(windows)]
        ClientMsg::ClipboardGet => {
            let text = crate::clipboard::read_text().unwrap_or_default();
            send(outbox, ServerMsg::ClipboardText { text });
        }
        // Non-Windows stubs so the trait stays exhaustive.
        #[cfg(not(windows))]
        ClientMsg::MouseMove { .. }
        | ClientMsg::MouseButton { .. }
        | ClientMsg::MouseScroll { .. }
        | ClientMsg::KeyText { .. }
        | ClientMsg::KeyEvent { .. }
        | ClientMsg::ClipboardSet { .. }
        | ClientMsg::ClipboardGet => {}

        // M6 file transfer messages are handled async upstream in
        // `run_connection` before this synchronous dispatch is reached.
        // Listing them here keeps the enum exhaustive.
        ClientMsg::FileTransferBegin { .. } | ClientMsg::FileTransferAbort { .. } => {
            // Unreachable in normal flow; if the dispatch ordering ever
            // changes upstream, fail loudly rather than silently dropping.
            warn!(
                "{peer_label}: file-transfer msg reached sync dispatch (bug)"
            );
        }
    }

    true
}

/// Helper: serialize + push onto the outbound channel. Errors are
/// intentionally swallowed — the channel only fails when the transport
/// pump has exited, in which case the next inbox.recv() will return
/// None and we'll tear down naturally.
pub fn send(outbox: &OutboundTx, msg: ServerMsg) {
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = outbox.send(Message::Text(text.into()));
    }
}

// ----------------------------------------------------------------------
// M6: phone → PC file transfer
// ----------------------------------------------------------------------

/// Per-transfer state kept in `run_connection`'s `HashMap` for the life
/// of one upload. Dropped on completion, abort, or peer disconnect —
/// the latter case removes the partial file from disk too.
pub struct FileTransferState {
    pub dest_path: std::path::PathBuf,
    pub file: tokio::fs::File,
    pub expected_size: u64,
    pub bytes_written: u64,
    /// We send chunks strictly in order, so the next chunk we accept
    /// must have `chunk_seq == next_expected_seq`. Out-of-order chunks
    /// would mean WS reordering (shouldn't happen on a single TCP
    /// stream) or buggy client; either way we fail the transfer rather
    /// than try to recover.
    pub next_expected_seq: u32,
}

async fn handle_file_begin(
    id: u32,
    name: &str,
    size: u64,
    peer_label: &str,
    transfers: &mut std::collections::HashMap<u32, FileTransferState>,
    outbox: &OutboundTx,
) {
    if transfers.contains_key(&id) {
        send(
            outbox,
            ServerMsg::FileTransferFailed {
                id,
                reason: "duplicate transfer id".into(),
            },
        );
        return;
    }
    // Soft cap: reject pathological sizes (>16 GiB) early. We accept any
    // smaller size — Windows NTFS handles up to 16 EiB, but 16 GiB is
    // already absurd for a remote-control accessory and serves as a
    // sanity check against bad clients.
    if size > 16 * 1024 * 1024 * 1024 {
        send(
            outbox,
            ServerMsg::FileTransferFailed {
                id,
                reason: "file too large (>16 GiB)".into(),
            },
        );
        return;
    }
    let dest_dir = match downloads_dir().await {
        Ok(p) => p,
        Err(e) => {
            send(
                outbox,
                ServerMsg::FileTransferFailed {
                    id,
                    reason: format!("downloads dir: {e:#}"),
                },
            );
            return;
        }
    };
    let safe_name = sanitize_filename(name);
    let dest_path = match unique_dest_path(&dest_dir, &safe_name).await {
        Ok(p) => p,
        Err(e) => {
            send(
                outbox,
                ServerMsg::FileTransferFailed {
                    id,
                    reason: format!("dest path: {e:#}"),
                },
            );
            return;
        }
    };
    let file = match tokio::fs::File::create(&dest_path).await {
        Ok(f) => f,
        Err(e) => {
            send(
                outbox,
                ServerMsg::FileTransferFailed {
                    id,
                    reason: format!("open {}: {e}", dest_path.display()),
                },
            );
            return;
        }
    };
    info!(
        "file transfer {id} from {peer_label} → {} ({} bytes expected)",
        dest_path.display(),
        size
    );
    transfers.insert(
        id,
        FileTransferState {
            dest_path: dest_path.clone(),
            file,
            expected_size: size,
            bytes_written: 0,
            next_expected_seq: 0,
        },
    );
    send(
        outbox,
        ServerMsg::FileTransferAccepted {
            id,
            dest_path: dest_path.to_string_lossy().into_owned(),
        },
    );
}

async fn handle_file_abort(
    id: u32,
    reason: &str,
    peer_label: &str,
    transfers: &mut std::collections::HashMap<u32, FileTransferState>,
) {
    if let Some(state) = transfers.remove(&id) {
        info!(
            "file transfer {id} from {peer_label} aborted: {reason} (partial \
             {} of {} bytes)",
            state.bytes_written, state.expected_size
        );
        drop(state.file);
        if let Err(e) = tokio::fs::remove_file(&state.dest_path).await {
            warn!(
                "remove partial {}: {e}",
                state.dest_path.display()
            );
        }
    }
}

async fn handle_file_chunk(
    bytes: &[u8],
    peer_label: &str,
    transfers: &mut std::collections::HashMap<u32, FileTransferState>,
    outbox: &OutboundTx,
) {
    let Some((id, seq, is_last, payload)) = crate::protocol::parse_file_chunk(bytes) else {
        warn!("{peer_label}: malformed file chunk ({} bytes)", bytes.len());
        return;
    };
    // Phase 1: pull the state out so we can mutate without keeping the
    // map borrowed; we put it back (or not, if this is the last chunk)
    // at the end.
    let mut state = match transfers.remove(&id) {
        Some(s) => s,
        None => {
            warn!("{peer_label}: chunk for unknown transfer id {id}");
            return;
        }
    };
    if seq != state.next_expected_seq {
        warn!(
            "{peer_label}: file {id}: out-of-order chunk (got {seq}, expected {})",
            state.next_expected_seq
        );
        let dest = state.dest_path.clone();
        drop(state.file);
        let _ = tokio::fs::remove_file(&dest).await;
        send(
            outbox,
            ServerMsg::FileTransferFailed {
                id,
                reason: format!("chunk out of order: got {seq}"),
            },
        );
        return;
    }
    use tokio::io::AsyncWriteExt;
    if let Err(e) = state.file.write_all(payload).await {
        warn!("{peer_label}: file {id} write_all: {e}");
        let dest = state.dest_path.clone();
        drop(state.file);
        let _ = tokio::fs::remove_file(&dest).await;
        send(
            outbox,
            ServerMsg::FileTransferFailed {
                id,
                reason: format!("write: {e}"),
            },
        );
        return;
    }
    state.bytes_written += payload.len() as u64;
    state.next_expected_seq = state.next_expected_seq.wrapping_add(1);

    if is_last {
        if let Err(e) = state.file.flush().await {
            warn!("{peer_label}: file {id} flush: {e}");
        }
        if state.expected_size != 0 && state.bytes_written != state.expected_size {
            warn!(
                "file transfer {id} size mismatch: wrote {} of {} bytes",
                state.bytes_written, state.expected_size
            );
        }
        let dest_display = state.dest_path.to_string_lossy().into_owned();
        info!(
            "file transfer {id} from {peer_label} done: {} bytes → {}",
            state.bytes_written, dest_display
        );
        drop(state.file);
        send(
            outbox,
            ServerMsg::FileTransferComplete {
                id,
                dest_path: dest_display,
            },
        );
        // state dropped, not re-inserted
    } else {
        transfers.insert(id, state);
    }
}

/// `%USERPROFILE%\Downloads\RemoteControl` on Windows, `$HOME/Downloads
/// /RemoteControl` elsewhere. Created if missing.
async fn downloads_dir() -> Result<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("USERPROFILE not set"))?
    } else {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME not set"))?
    };
    let dir = base.join("Downloads").join("RemoteControl");
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// Strip dangerous characters from a phone-provided file name. Returns
/// a safe basename (no path separators, no leading/trailing whitespace,
/// max 200 chars). An empty or all-bad input falls back to a generic
/// `received.bin` so the upload still has somewhere to land.
fn sanitize_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            !(*c == '/' || *c == '\\' || *c == '\0' || *c == ':' || *c == '*'
                || *c == '?' || *c == '"' || *c == '<' || *c == '>' || *c == '|')
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "received.bin".to_string();
    }
    if trimmed.chars().count() > 200 {
        // Truncate but keep extension if present.
        let (stem, ext) = match trimmed.rsplit_once('.') {
            Some((s, e)) if e.len() < 32 => (s, format!(".{e}")),
            _ => (trimmed, String::new()),
        };
        let take = 200 - ext.chars().count();
        let stem_short: String = stem.chars().take(take).collect();
        return format!("{stem_short}{ext}");
    }
    trimmed.to_string()
}

/// Resolve a non-colliding destination path. If `dir/name` already
/// exists, try `dir/name (1)`, `dir/name (2)`, etc. up to 999. The
/// stem-vs-extension split preserves `.txt` etc.
async fn unique_dest_path(dir: &std::path::Path, name: &str) -> Result<std::path::PathBuf> {
    let initial = dir.join(name);
    if tokio::fs::metadata(&initial).await.is_err() {
        return Ok(initial);
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..=999 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if tokio::fs::metadata(&candidate).await.is_err() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no unique path under {} for {name}", dir.display())
}

pub fn send_error(outbox: &OutboundTx, code: ErrorCode, m: &str) {
    send(
        outbox,
        ServerMsg::Error {
            code,
            msg: m.to_string(),
        },
    );
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
