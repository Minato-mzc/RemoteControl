//! Per-connection WebSocket loop.
//!
//! Each TCP accept gets its own task that:
//!   1. handshakes (M1 protocol — see [`PROTOCOL.md`](../../docs/PROTOCOL.md))
//!   2. handles control messages (`stream_request`, `keyframe_request`, `ping`)
//!   3. drains an active stream's packets and ships them as binary frames
//!
//! Concurrent inbound/outbound is done via `tokio::select!` over the WS read
//! half + the stream packet receiver. The sink is owned by this task too —
//! all writes serialize naturally without locks.

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

use crate::config::{Config, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION, SERVER_VERSION};
#[cfg(windows)]
use crate::input;
use crate::pairing::{PairingStore, VerifyResult};
use crate::protocol::{ClientMsg, ErrorCode, ServerInfo, ServerMsg, StreamStopReason};
use crate::stream::{start_stream, RequestedCodec, StreamHandle, StreamRequestParams};
use crate::trusted_devices::{TrustedDevicesStore, VerifyOutcome};

type HmacSha256 = Hmac<Sha256>;
type WsSink = SplitSink<WebSocketStream<TcpStream>, Message>;

pub async fn run(
    host: String,
    port: u16,
    pairing: PairingStore,
    trusted: TrustedDevicesStore,
    cfg: Config,
) -> Result<()> {
    let bind = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind).await?;
    info!("WebSocket listening on {bind} (advertised as {host}:{port})");

    let pairing = Arc::new(pairing);
    let trusted = Arc::new(trusted);
    let cfg = Arc::new(cfg);

    loop {
        let (tcp, peer) = listener.accept().await?;
        let pairing = pairing.clone();
        let trusted = trusted.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp, peer, pairing, trusted, cfg).await {
                warn!("connection from {peer} ended: {e:#}");
            }
        });
    }
}

async fn handle_connection(
    tcp: TcpStream,
    peer: std::net::SocketAddr,
    pairing: Arc<PairingStore>,
    trusted: Arc<TrustedDevicesStore>,
    cfg: Arc<Config>,
) -> Result<()> {
    let ws = accept_async(tcp).await?;
    let (mut sink, mut stream) = ws.split();
    info!("peer connected: {peer}");

    let mut authenticated = false;
    let mut active_stream: Option<StreamHandle> = None;

    loop {
        // Build the (optional) packet-recv future fresh each iteration so its
        // borrow on `active_stream` doesn't outlive the select.
        let next_pkt = async {
            match active_stream.as_mut() {
                Some(s) => s.packets.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            ws_msg = stream.next() => {
                let msg = match ws_msg {
                    None => break,
                    Some(Err(e)) => { warn!("ws read from {peer}: {e}"); break; }
                    Some(Ok(m)) => m,
                };
                match msg {
                    Message::Text(text) => {
                        match serde_json::from_str::<ClientMsg>(text.as_str()) {
                            Err(e) => {
                                warn!("malformed from {peer}: {e}");
                                send_error(&mut sink, ErrorCode::Malformed, "cannot parse message").await?;
                                break;
                            }
                            Ok(parsed) => {
                                let cont = handle_client_msg(
                                    parsed, peer, &pairing, &trusted, &cfg,
                                    &mut authenticated, &mut active_stream,
                                    &mut sink,
                                ).await?;
                                if !cont { break; }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(p) => {
                        if sink.send(Message::Pong(p)).await.is_err() { break; }
                    }
                    _ => {}
                }
            }

            maybe_pkt = next_pkt => {
                let Some(bin) = maybe_pkt else {
                    // Stream ended on its own (encoder error or worker exit).
                    if let Some(handle) = active_stream.take() {
                        let stop_msg = ServerMsg::StreamStopped {
                            stream_id: handle.stream_id.clone(),
                            reason: StreamStopReason::EncoderError,
                            msg: "stream worker exited".to_string(),
                        };
                        let _ = send(&mut sink, stop_msg).await;
                    }
                    continue;
                };
                if sink.send(Message::Binary(bin.into())).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(handle) = active_stream.take() {
        handle.stop();
    }
    info!("peer disconnected: {peer} (authenticated={authenticated})");
    Ok(())
}

/// Returns Ok(false) when the caller should drop the connection (handshake
/// failure, fatal protocol error, etc.). Ok(true) means keep going.
async fn handle_client_msg(
    msg: ClientMsg,
    peer: std::net::SocketAddr,
    pairing: &PairingStore,
    trusted: &TrustedDevicesStore,
    cfg: &Config,
    authenticated: &mut bool,
    active_stream: &mut Option<StreamHandle>,
    sink: &mut WsSink,
) -> Result<bool> {
    match msg {
        ClientMsg::Hello { v, c, nonce, client } => {
            if !(MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&v) {
                send_error(
                    sink,
                    ErrorCode::VersionMismatch,
                    &format!(
                        "server accepts v={MIN_SUPPORTED_VERSION}..={PROTOCOL_VERSION}, got v={v}"
                    ),
                )
                .await?;
                return Ok(false);
            }
            match pairing.verify_and_consume(&c) {
                VerifyResult::Ok => {
                    let nonce_bytes = match URL_SAFE_NO_PAD.decode(nonce.as_bytes()) {
                        Ok(b) => b,
                        Err(_) => {
                            send_error(sink, ErrorCode::Malformed, "nonce not base64url").await?;
                            return Ok(false);
                        }
                    };
                    let key = pairing.key();
                    let mut mac = HmacSha256::new_from_slice(&key)
                        .expect("HMAC accepts any key length");
                    mac.update(&nonce_bytes);
                    let hmac_hex = hex_encode(&mac.finalize().into_bytes());

                    let session = uuid::Uuid::new_v4().to_string();
                    // Mint a long-lived trust token so the phone doesn't have to
                    // scan a QR every time. Failure here is non-fatal — the
                    // pairing succeeded, we just won't enable seamless
                    // reconnect for this device. Phone falls back to QR.
                    let (trust_token, device_id) = match trusted.mint(client.name.clone()) {
                        Ok((dev_id, token)) => {
                            info!(
                                "minted trust token  device_id={dev_id}  device_name={:?}",
                                client.name
                            );
                            (Some(token), Some(dev_id))
                        }
                        Err(e) => {
                            warn!("trusted_devices.mint failed: {e:#}");
                            (None, None)
                        }
                    };
                    info!(
                        "handshake OK  peer={peer} session={session} client={:?}",
                        client
                    );
                    send(
                        sink,
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
                    )
                    .await?;
                    *authenticated = true;
                }
                VerifyResult::BadCode => {
                    send_error(sink, ErrorCode::BadPairingCode, "wrong code").await?;
                    return Ok(false);
                }
                VerifyResult::Expired => {
                    send_error(sink, ErrorCode::CodeExpired, "code expired").await?;
                    return Ok(false);
                }
                VerifyResult::Used => {
                    send_error(sink, ErrorCode::CodeUsed, "code already used").await?;
                    return Ok(false);
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
                    sink,
                    ErrorCode::VersionMismatch,
                    &format!(
                        "server accepts v={MIN_SUPPORTED_VERSION}..={PROTOCOL_VERSION}, got v={v}"
                    ),
                )
                .await?;
                return Ok(false);
            }
            match trusted.verify(&device_id, &token) {
                Ok(VerifyOutcome::Ok { device_name }) => {
                    let session = uuid::Uuid::new_v4().to_string();
                    info!(
                        "trusted reconnect OK  peer={peer} session={session}  device={device_name:?} (id={device_id}) client={:?}",
                        client
                    );
                    send(
                        sink,
                        ServerMsg::Welcome {
                            session,
                            server: ServerInfo {
                                name: cfg.server_name.clone(),
                                os: cfg.os.clone(),
                                version: SERVER_VERSION.to_string(),
                            },
                            // No HMAC challenge for trusted reconnect — the
                            // token itself is the authentication factor. The
                            // field stays in the schema for QR-path callers.
                            hmac: String::new(),
                            // Don't re-issue. Phone keeps the same token.
                            trust_token: None,
                            device_id: None,
                        },
                    )
                    .await?;
                    *authenticated = true;
                }
                Ok(VerifyOutcome::UnknownDevice) => {
                    info!(
                        "trusted reconnect rejected (unknown device_id={device_id}) peer={peer}"
                    );
                    send_error(sink, ErrorCode::UnknownDevice, "device not trusted; please re-pair via QR").await?;
                    return Ok(false);
                }
                Ok(VerifyOutcome::BadToken) => {
                    warn!(
                        "trusted reconnect rejected (bad token for device_id={device_id}) peer={peer}"
                    );
                    send_error(sink, ErrorCode::BadTrustToken, "trust token mismatch").await?;
                    return Ok(false);
                }
                Err(e) => {
                    warn!("trusted_devices.verify error: {e:#}");
                    send_error(sink, ErrorCode::Malformed, "internal trust check failed").await?;
                    return Ok(false);
                }
            }
        }

        ClientMsg::Ping { ts } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            send(sink, ServerMsg::Pong { ts }).await?;
        }

        ClientMsg::StreamRequest {
            codec,
            max_bitrate_kbps,
            max_fps,
            prefer_keyframe_interval_ms,
        } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            let requested_codec = match codec.to_ascii_lowercase().as_str() {
                "h264" => RequestedCodec::H264,
                "hevc" | "h265" => RequestedCodec::Hevc,
                _ => {
                    send_error(
                        sink,
                        ErrorCode::StreamUnavailable,
                        &format!("only h264/hevc supported, got {codec}"),
                    )
                    .await?;
                    return Ok(true);
                }
            };
            if active_stream.is_some() {
                send_error(
                    sink,
                    ErrorCode::StreamAlreadyRunning,
                    "stop the current stream first",
                )
                .await?;
                return Ok(true);
            }

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
                    send(sink, started).await?;
                    *active_stream = Some(handle);
                }
                Err(e) => {
                    warn!("start_stream failed: {e:#}");
                    send_error(
                        sink,
                        ErrorCode::StreamUnavailable,
                        &format!("could not start stream: {e}"),
                    )
                    .await?;
                }
            }
        }

        ClientMsg::StreamStop { stream_id } => {
            if let Some(handle) = active_stream.take() {
                let id = handle.stream_id.clone();
                if matches!(stream_id, Some(ref s) if s != &id) {
                    // Different stream id — ignore but acknowledge by ending what we have.
                }
                drop(handle); // closes channel; worker exits; Drop also sets stop flag
                send(
                    sink,
                    ServerMsg::StreamStopped {
                        stream_id: id,
                        reason: StreamStopReason::ClientRequested,
                        msg: String::new(),
                    },
                )
                .await?;
            }
        }

        ClientMsg::KeyframeRequest { stream_id: _ } => {
            if let Some(s) = active_stream.as_ref() {
                s.force_keyframe();
            }
        }

        // ---- M3: mouse input ----

        ClientMsg::MouseMove { x, y } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            // High-frequency, kept at debug to avoid spam.
            debug!("mouse_move x={x:.4} y={y:.4}");
            #[cfg(windows)]
            if let Err(e) = input::move_to(x, y) {
                warn!("mouse_move failed: {e:#}");
            }
        }

        ClientMsg::MouseButton { button, down } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            info!("mouse_button {:?} down={}", button, down);
            #[cfg(windows)]
            if let Err(e) = input::button(button, down) {
                warn!("mouse_button failed: {e:#}");
            }
        }

        ClientMsg::MouseScroll { dx, dy } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            info!("mouse_scroll dx={dx} dy={dy}");
            #[cfg(windows)]
            if let Err(e) = input::scroll(dx, dy) {
                warn!("mouse_scroll failed: {e:#}");
            }
        }

        // ---- M3.5: keyboard input ----

        ClientMsg::KeyText { text } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            // Truncate to a sane length for the log line; the full string still gets injected.
            let preview: String = text.chars().take(32).collect();
            info!("key_text len={} preview={preview:?}", text.chars().count());
            #[cfg(windows)]
            if let Err(e) = input::type_unicode(&text) {
                warn!("key_text failed: {e:#}");
            }
        }

        ClientMsg::KeyEvent { vk, down } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            info!("key_event vk={vk} down={down}");
            #[cfg(windows)]
            if let Err(e) = input::vkey(vk, down) {
                warn!("key_event failed: {e:#}");
            }
        }

        // ---- M8: clipboard sync ----

        ClientMsg::ClipboardSet { text } => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            let preview: String = text.chars().take(32).collect();
            info!("clipboard_set len={} preview={preview:?}", text.chars().count());
            #[cfg(windows)]
            if let Err(e) = crate::clipboard::write_text(&text) {
                warn!("clipboard_set failed: {e:#}");
            }
        }

        ClientMsg::ClipboardGet => {
            if !*authenticated {
                send_error(sink, ErrorCode::NotAuthenticated, "hello first").await?;
                return Ok(false);
            }
            #[cfg(windows)]
            {
                match crate::clipboard::read_text() {
                    Ok(text) => {
                        let preview: String = text.chars().take(32).collect();
                        info!(
                            "clipboard_get -> len={} preview={preview:?}",
                            text.chars().count()
                        );
                        send(sink, ServerMsg::ClipboardText { text }).await?;
                    }
                    Err(e) => {
                        warn!("clipboard_get failed: {e:#}");
                        send(
                            sink,
                            ServerMsg::ClipboardText {
                                text: String::new(),
                            },
                        )
                        .await?;
                    }
                }
            }
        }
    }

    Ok(true)
}

async fn send(sink: &mut WsSink, msg: ServerMsg) -> Result<()> {
    let text = serde_json::to_string(&msg)?;
    sink.send(Message::Text(text.into())).await?;
    Ok(())
}

async fn send_error(sink: &mut WsSink, code: ErrorCode, msg: &str) -> Result<()> {
    send(
        sink,
        ServerMsg::Error {
            code,
            msg: msg.to_string(),
        },
    )
    .await
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}
