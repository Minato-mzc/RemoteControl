//! Outbound relay client.
//!
//! When the user wants their PC reachable from outside the LAN (mobile
//! data, different Wi-Fi, etc.) they deploy `remotecontrol-relay` on a
//! VPS and configure this module to dial it. The relay then routes
//! phone WebSockets to us through one long-lived outbound WebSocket.
//!
//! ## Lifecycle
//! 1. **Provision** — first-ever launch with relay configured: POST
//!    `/v1/host/register` to mint `(host_id, host_token)`. Save them
//!    plus the relay's HTTPS URL into `%LOCALAPPDATA%/RemoteControl/relay.toml`.
//! 2. **Connect** — every subsequent launch (or after the WS drops):
//!    open `wss://<relay>/v1/host?host_id=&host_token=`, keep it open.
//! 3. **Multiplex** — incoming `client_open {client_id}` frames spawn a
//!    new *virtual* connection that reuses [`crate::ws_server`]'s
//!    handshake / message-handling code paths via the in-process
//!    [`VirtualPeer`] adapter.
//!
//! ## Why we don't speak WebSocket inside the tunnel
//! The relay strips WebSocket framing on the phone side and re-emits
//! payloads as JSON tunnel frames. The PC reconstructs `Message::Text`
//! / `Message::Binary` and feeds them straight into the existing
//! per-connection state machine — no second WebSocket layer needed,
//! no double base64ing of video frames.

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async, connect_async, MaybeTlsStream};
use tracing::{info, warn};

use crate::config::Config;
use crate::connection::{run_connection, OutboundTx};
use crate::pairing::PairingStore;
use crate::trusted_devices::TrustedDevicesStore;

/// On-disk record produced by [`provision`] and consumed on every
/// subsequent boot. Stored next to `trusted_devices.json` so the user
/// has a single config dir to wipe if they ever want to "factory
/// reset" their relay/trust state.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RelayConfig {
    /// Public-facing URL of the relay. The HTTP form is used for the
    /// one-shot register call (`{base}/v1/host/register`); the WS form
    /// (`wss://...` derived by replacing the scheme) is used for the
    /// long-lived host connection. We store one URL so the user only
    /// has to type one thing into setup.
    pub base_url: String,
    /// Stable host identifier the relay assigned us. Goes verbatim into
    /// the `rcrelay://` QR payload so the phone knows where to land.
    pub host_id: String,
    /// 256-bit secret authenticating *us* to the relay on every host
    /// WebSocket open. **NEVER** put this in the QR — it stays on the
    /// PC. Relay verifies via SHA-256(token).
    pub host_token: String,
}

#[cfg(target_os = "windows")]
fn config_path() -> Result<PathBuf> {
    let local_appdata = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .context("LOCALAPPDATA / APPDATA not set")?;
    Ok(local_appdata.join("RemoteControl").join("relay.toml"))
}

#[cfg(not(target_os = "windows"))]
fn config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(base.join("RemoteControl").join("relay.toml"))
}

/// Read existing config from disk, returning `Ok(None)` when the file
/// hasn't been created yet (= relay never configured) so the caller
/// can prompt for setup.
pub fn load() -> Result<Option<RelayConfig>> {
    let path = config_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let text = String::from_utf8(bytes)
        .with_context(|| format!("relay.toml not utf-8: {}", path.display()))?;
    let cfg: RelayConfig = toml::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
}

pub fn save(cfg: &RelayConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized =
        toml::to_string_pretty(cfg).context("serialize relay.toml")?;
    std::fs::write(&path, serialized)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// First-time setup: ask the relay to mint a `(host_id, host_token)`
/// for this PC, then persist the result. Idempotent only in the sense
/// that calling it twice creates two distinct host records — caller
/// should check `load()` before calling.
pub async fn provision(base_url: &str, server_name: &str) -> Result<RelayConfig> {
    use reqwest::Client;
    #[derive(Serialize)]
    struct Req<'a> {
        name: &'a str,
    }
    #[derive(Deserialize)]
    struct Resp {
        host_id: String,
        host_token: String,
    }
    // Bypass system HTTP_PROXY / HTTPS_PROXY env vars. Many users have
    // these set globally to point at a Clash/V2Ray-style local proxy,
    // which then returns 502 for LAN-bound URLs (the proxy refuses to
    // tunnel into the local network). The relay URL is whatever the
    // user explicitly typed, so we want a direct connection regardless.
    let client = Client::builder()
        .no_proxy()
        .build()
        .context("build reqwest client")?;
    let url = format!("{}/v1/host/register", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&Req { name: server_name })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?
        .error_for_status()
        .context("relay rejected register request")?;
    let body: Resp = resp.json().await.context("parse register response")?;
    let cfg = RelayConfig {
        base_url: base_url.trim_end_matches('/').to_string(),
        host_id: body.host_id,
        host_token: body.host_token,
    };
    save(&cfg).context("persist relay.toml")?;
    Ok(cfg)
}

// ============================================================================
// Tunnel framing — wire-compatible with relay/src/main.rs::TunnelFrame.
// ============================================================================
//
// Binary header layout:
//   [0]      msg_type: 1=Open, 2=Close, 3=Data
//   [1..37]  client_id (36-byte ASCII UUIDv4)
//   [37]     text_flag (1 if payload is Text, 0 if Binary; reserved=0 for Open/Close)
//   [38..]   payload (Data only)
//
// Same scheme on both sides; switching off JSON+base64 was driven by the
// 33% bandwidth inflation choking the cross-network path on residential
// upstream links.

#[derive(Debug)]
pub enum TunnelFrame {
    ClientOpen {
        client_id: String,
    },
    ClientClose {
        client_id: String,
    },
    Data {
        client_id: String,
        text: bool,
        payload: Vec<u8>,
    },
}

const TUNNEL_HEADER_LEN: usize = 38;
const TF_OPEN: u8 = 1;
const TF_CLOSE: u8 = 2;
const TF_DATA: u8 = 3;

impl TunnelFrame {
    fn encode(&self) -> Vec<u8> {
        match self {
            TunnelFrame::ClientOpen { client_id } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN);
                buf.push(TF_OPEN);
                push_uuid(&mut buf, client_id);
                buf.push(0);
                buf
            }
            TunnelFrame::ClientClose { client_id } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN);
                buf.push(TF_CLOSE);
                push_uuid(&mut buf, client_id);
                buf.push(0);
                buf
            }
            TunnelFrame::Data {
                client_id,
                text,
                payload,
            } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN + payload.len());
                buf.push(TF_DATA);
                push_uuid(&mut buf, client_id);
                buf.push(if *text { 1 } else { 0 });
                buf.extend_from_slice(payload);
                buf
            }
        }
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < TUNNEL_HEADER_LEN {
            return None;
        }
        let mty = bytes[0];
        let client_id = std::str::from_utf8(&bytes[1..37]).ok()?.to_string();
        let flag = bytes[37];
        match mty {
            TF_OPEN => Some(TunnelFrame::ClientOpen { client_id }),
            TF_CLOSE => Some(TunnelFrame::ClientClose { client_id }),
            TF_DATA => Some(TunnelFrame::Data {
                client_id,
                text: flag != 0,
                payload: bytes[TUNNEL_HEADER_LEN..].to_vec(),
            }),
            _ => None,
        }
    }
}

fn push_uuid(buf: &mut Vec<u8>, uuid: &str) {
    let mut bytes = [b'0'; 36];
    let src = uuid.as_bytes();
    let n = src.len().min(36);
    bytes[..n].copy_from_slice(&src[..n]);
    buf.extend_from_slice(&bytes);
}

// ============================================================================
// RelayClient — long-lived host WS + per-phone session multiplexer.
// ============================================================================

/// Per-phone-session driver. Owns the side that pushes frames into
/// [`crate::connection::run_connection`] and pulls outbound frames from
/// it. The relay's host-WS loop matches incoming `TunnelFrame::Data`
/// to a phone session by its `client_id` and forwards to the right one.
struct PhoneSession {
    /// Sink the host loop writes phone-originated WS messages into;
    /// drains into `connection::run_connection`'s inbox.
    inbox_tx: mpsc::UnboundedSender<Message>,
}

pub struct RelayClient {
    cfg: RelayConfig,
}

impl RelayClient {
    pub fn new(cfg: RelayConfig) -> Self {
        Self { cfg }
    }

    /// Run one long-lived host WebSocket loop. Returns when the WS
    /// drops or the relay rejects us; the caller should retry with
    /// backoff. Spawns one background task per phone connecting through
    /// the tunnel.
    pub async fn run(
        self,
        pairing: Arc<PairingStore>,
        trusted: Arc<TrustedDevicesStore>,
        cfg: Arc<Config>,
        file_send_bridge: Arc<crate::file_send::FileSendBridge>,
    ) -> Result<()> {
        // Translate http(s) base URL into ws(s) for the long-lived WS.
        // We accept either form so the user's relay.toml can store
        // whichever they typed.
        let ws_url = host_ws_url(&self.cfg)?;
        info!("dialing relay  url={ws_url}  host_id={}", self.cfg.host_id);

        // For plain `ws://` we manually open the TCP socket and hand it
        // to `client_async`. Why bother instead of using `connect_async`?
        // Because `connect_async` resolves the host through whatever
        // proxy machinery the underlying stack picks up — on Windows
        // with a system-wide proxy like Clash exporting `HTTP_PROXY=
        // http://127.0.0.1:7897`, the dial gets steered into that proxy
        // and silently times out (the proxy refuses to forward to
        // private/CN IPs in many configurations). Going TCP-direct here
        // sidesteps any environment proxy lookup entirely so the
        // launcher script doesn't have to remember to clear half a
        // dozen env vars on every run. `wss://` still goes through
        // `connect_async` because we'd otherwise have to wire up rustls
        // ourselves; production TLS deploys are unlikely to also have a
        // localhost proxy intercepting their loopback traffic.
        let (ws, _resp) = if let Some(addr) = parse_ws_authority(&ws_url) {
            let tcp = TcpStream::connect(&addr)
                .await
                .with_context(|| format!("relay tcp connect {addr}"))?;
            let request = ws_url
                .as_str()
                .into_client_request()
                .with_context(|| format!("build ws request {ws_url}"))?;
            // Wrap as `MaybeTlsStream::Plain` so the resulting
            // `WebSocketStream` shape matches `connect_async`'s return
            // type — needed only because both branches of this if/else
            // need to assign into the same `(ws, _resp)` binding.
            client_async(request, MaybeTlsStream::Plain(tcp))
                .await
                .with_context(|| format!("relay ws upgrade {ws_url}"))?
        } else {
            connect_async(&ws_url)
                .await
                .with_context(|| format!("relay dial {ws_url}"))?
        };
        info!(
            "relay connected  host_id={}  waiting for phone sessions",
            self.cfg.host_id
        );
        let (mut ws_sink, mut ws_stream) = ws.split();

        // Per-host phone-session map. The host-WS reader pushes inbound
        // tunnel data to the matching session; the writer drains a
        // single outbound queue (one per host WS) merging output from
        // every active phone session.
        let sessions: Arc<Mutex<HashMap<String, PhoneSession>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (host_out_tx, mut host_out_rx) =
            mpsc::unbounded_channel::<TunnelFrame>();

        // Reader: relay → us. Demuxes `client_open / close / data` into
        // per-phone session pumps; spawns a `run_connection` task per
        // ClientOpen.
        let reader_pairing = pairing.clone();
        let reader_trusted = trusted.clone();
        let reader_cfg = cfg.clone();
        let reader_sessions = sessions.clone();
        let reader_out_tx = host_out_tx.clone();
        let reader_bridge = file_send_bridge.clone();

        let reader = tokio::spawn(async move {
            while let Some(item) = ws_stream.next().await {
                let bytes = match item {
                    Ok(Message::Binary(b)) => b,
                    Ok(Message::Close(_)) | Err(_) => break,
                    // Text on the host side is unexpected after the
                    // binary-tunnel switch; ignore it (could be a leftover
                    // from a mismatched relay version).
                    _ => continue,
                };
                let frame = match TunnelFrame::decode(&bytes) {
                    Some(f) => f,
                    None => {
                        warn!("relay malformed tunnel frame ({} bytes)", bytes.len());
                        continue;
                    }
                };
                match frame {
                    TunnelFrame::ClientOpen { client_id } => {
                        let (inbox_tx, inbox_rx) =
                            mpsc::unbounded_channel::<Message>();
                        let (outbox_tx, mut outbox_rx) =
                            mpsc::unbounded_channel::<Message>();
                        // Bounded video channel — drop-on-full so a
                        // slow downstream can't grow a backlog of
                        // stale video frames. See connection.rs for
                        // the full rationale.
                        let (outbox_video_tx, mut outbox_video_rx) =
                            mpsc::channel::<Message>(
                                crate::connection::OUTBOUND_VIDEO_CAP,
                            );
                        // Bounded bulk channel (blocking on full) so
                        // file sends back-pressure the streamer.
                        let (outbox_bulk_tx, mut outbox_bulk_rx) =
                            mpsc::channel::<Message>(
                                crate::connection::OUTBOUND_BULK_CAP,
                            );
                        reader_sessions.lock().await.insert(
                            client_id.clone(),
                            PhoneSession { inbox_tx },
                        );

                        // Logic: the same state-machine the LAN path uses.
                        let label = format!("relay/{client_id}");
                        let logic_pairing = reader_pairing.clone();
                        let logic_trusted = reader_trusted.clone();
                        let logic_cfg = reader_cfg.clone();
                        let logic_bridge = reader_bridge.clone();
                        tokio::spawn(async move {
                            if let Err(e) = run_connection(
                                label,
                                inbox_rx,
                                outbox_tx,
                                outbox_video_tx,
                                outbox_bulk_tx,
                                logic_pairing,
                                logic_trusted,
                                logic_cfg,
                                logic_bridge,
                            )
                            .await
                            {
                                warn!("tunnel session ended: {e:#}");
                            }
                        });

                        // Outbound pump: this session's `OutboundTx`/
                        // `OutboundBulkTx` drain here. We wrap each
                        // Message into a `TunnelFrame::Data` aimed at
                        // `client_id` and forward to the host writer.
                        // Biased select on `outbox_rx` (fast) first so
                        // a backed-up file send can't delay control or
                        // video traffic.
                        let writer_out_tx = reader_out_tx.clone();
                        let writer_sessions = reader_sessions.clone();
                        let writer_cid = client_id.clone();
                        tokio::spawn(async move {
                            // Result of pulling one Message off either
                            // queue and turning it into a tunnel frame.
                            // `Skip` lets us silently drop unsupported
                            // variants without breaking the loop.
                            enum Pumped { Frame(TunnelFrame), Skip, Stop }
                            let pump_one = |m: Message| -> Pumped {
                                let (text, bytes) = match m {
                                    Message::Text(s) => (true, s.as_bytes().to_vec()),
                                    Message::Binary(b) => (false, b.to_vec()),
                                    Message::Close(_) => return Pumped::Stop,
                                    _ => return Pumped::Skip,
                                };
                                Pumped::Frame(TunnelFrame::Data {
                                    client_id: writer_cid.clone(),
                                    text,
                                    payload: bytes,
                                })
                            };
                            loop {
                                let pumped = tokio::select! {
                                    biased;
                                    m = outbox_rx.recv() => {
                                        match m { Some(m) => pump_one(m), None => break }
                                    }
                                    m = outbox_video_rx.recv() => {
                                        match m { Some(m) => pump_one(m), None => break }
                                    }
                                    m = outbox_bulk_rx.recv() => {
                                        match m { Some(m) => pump_one(m), None => break }
                                    }
                                    else => break,
                                };
                                let frame = match pumped {
                                    Pumped::Frame(f) => f,
                                    Pumped::Skip => continue,
                                    Pumped::Stop => break,
                                };
                                if writer_out_tx.send(frame).is_err() {
                                    break;
                                }
                            }
                            // Logic side dropped its OutboundTx → session over.
                            writer_sessions.lock().await.remove(&writer_cid);
                        });
                    }

                    TunnelFrame::ClientClose { client_id } => {
                        // Remove the session — its inbox_tx drops, run_connection
                        // sees None on next recv, exits cleanly.
                        reader_sessions.lock().await.remove(&client_id);
                    }

                    TunnelFrame::Data {
                        client_id,
                        text,
                        payload,
                    } => {
                        let msg = if text {
                            match String::from_utf8(payload) {
                                Ok(s) => Message::Text(s.into()),
                                Err(_) => continue,
                            }
                        } else {
                            Message::Binary(payload.into())
                        };
                        if let Some(sess) = reader_sessions.lock().await.get(&client_id) {
                            let _ = sess.inbox_tx.send(msg);
                        }
                    }
                }
            }
        });

        // Writer: us → relay. Binary tunnel frames go straight onto the
        // wire — one Message::Binary per TunnelFrame, no JSON envelope.
        //
        // Also drives a 30 s application-level Ping. The PC↔relay WS
        // can sit idle for many minutes between phone sessions; some
        // middleboxes on the path (carrier-grade NAT, VPS security
        // group with idle-flow eviction) silently drop TCP after a few
        // minutes of no traffic, and neither tokio-tungstenite (this
        // side) nor axum (relay side) auto-pings. A Ping every 30 s
        // keeps the flow warm; if the Ping itself can't be written —
        // because TCP is genuinely dead — we break and propagate the
        // error so the outer reconnect loop in `lib.rs` reopens the WS.
        let mut keepalive = tokio::time::interval(
            std::time::Duration::from_secs(30),
        );
        // First tick fires immediately by default; skip it so we don't
        // ping before the relay has even acknowledged the upgrade.
        keepalive.tick().await;
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = host_out_rx.recv() => {
                        let Some(frame) = frame else { break };
                        let bytes = frame.encode();
                        if ws_sink
                            .send(Message::Binary(bytes.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if ws_sink
                            .send(Message::Ping(Vec::new().into()))
                            .await
                            .is_err()
                        {
                            warn!("relay keepalive ping failed — tunnel is dead");
                            break;
                        }
                    }
                }
            }
            let _ = ws_sink.close().await;
        });

        // Whichever side dies first, drop the other.
        tokio::select! {
            _ = reader => {}
            _ = writer => {}
        };
        info!("relay disconnected  host_id={}", self.cfg.host_id);
        Ok(())
    }
}

/// Convert a `http(s)://relay.example.com` base URL into the WS form
/// for the long-lived host endpoint, with auth params already included.
/// If `ws_url` is a plain `ws://host:port/...` URL, return the
/// `host:port` authority — caller can `TcpStream::connect` to it
/// directly, bypassing any proxy environment variables that might
/// otherwise hijack the dial. Returns `None` for `wss://` so the caller
/// falls back to `connect_async`'s built-in TLS path.
///
/// Doesn't validate or canonicalize beyond what the relay's QR
/// generator already produces; specifically assumes the URL has an
/// explicit port (which our `qr.rs` guarantees, see
/// `save_qr_html_and_open` for why).
fn parse_ws_authority(ws_url: &str) -> Option<String> {
    let rest = ws_url.strip_prefix("ws://")?;
    // Keep everything before the first '/' or '?' — that's the authority.
    let end = rest.find(|c| c == '/' || c == '?').unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || !authority.contains(':') {
        // No explicit port → can't `TcpStream::connect`. Defer to
        // `connect_async`, which will fail with a more actionable
        // error than us guessing at port 80 vs 443.
        return None;
    }
    Some(authority.to_string())
}

fn host_ws_url(cfg: &RelayConfig) -> Result<String> {
    let base = cfg.base_url.trim_end_matches('/');
    let scheme = if base.starts_with("https://") {
        "wss"
    } else if base.starts_with("http://") {
        "ws"
    } else {
        anyhow::bail!("relay base_url must start with http:// or https://, got {base}");
    };
    let host_part = base
        .splitn(2, "://")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed relay base_url"))?;
    Ok(format!(
        "{scheme}://{host_part}/v1/host?host_id={}&host_token={}",
        cfg.host_id, cfg.host_token
    ))
}
