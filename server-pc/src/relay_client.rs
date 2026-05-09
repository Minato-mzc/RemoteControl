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
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
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
    let client = Client::new();
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum TunnelFrame {
    #[serde(rename = "client_open")]
    ClientOpen { client_id: String },
    #[serde(rename = "client_close")]
    ClientClose { client_id: String },
    #[serde(rename = "data")]
    Data {
        client_id: String,
        text: bool,
        payload_b64: String,
    },
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
    ) -> Result<()> {
        // Translate http(s) base URL into ws(s) for the long-lived WS.
        // We accept either form so the user's relay.toml can store
        // whichever they typed.
        let ws_url = host_ws_url(&self.cfg)?;
        info!("dialing relay  url={ws_url}  host_id={}", self.cfg.host_id);

        let (ws, _resp) = connect_async(&ws_url)
            .await
            .with_context(|| format!("relay dial {ws_url}"))?;
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

        let reader = tokio::spawn(async move {
            while let Some(item) = ws_stream.next().await {
                let msg = match item {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };
                let frame: TunnelFrame = match serde_json::from_str(&msg) {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("relay bad tunnel frame: {e}");
                        continue;
                    }
                };
                match frame {
                    TunnelFrame::ClientOpen { client_id } => {
                        let (inbox_tx, inbox_rx) =
                            mpsc::unbounded_channel::<Message>();
                        let (outbox_tx, mut outbox_rx) =
                            mpsc::unbounded_channel::<Message>();
                        reader_sessions.lock().await.insert(
                            client_id.clone(),
                            PhoneSession { inbox_tx },
                        );

                        // Logic: the same state-machine the LAN path uses.
                        let label = format!("relay/{client_id}");
                        let logic_pairing = reader_pairing.clone();
                        let logic_trusted = reader_trusted.clone();
                        let logic_cfg = reader_cfg.clone();
                        tokio::spawn(async move {
                            if let Err(e) = run_connection(
                                label,
                                inbox_rx,
                                outbox_tx,
                                logic_pairing,
                                logic_trusted,
                                logic_cfg,
                            )
                            .await
                            {
                                warn!("tunnel session ended: {e:#}");
                            }
                        });

                        // Outbound pump: this session's `OutboundTx` drains
                        // here and we wrap into a TunnelFrame::Data targeted
                        // at the same client_id, sending to the host writer.
                        let writer_out_tx = reader_out_tx.clone();
                        let writer_sessions = reader_sessions.clone();
                        let writer_cid = client_id.clone();
                        tokio::spawn(async move {
                            while let Some(m) = outbox_rx.recv().await {
                                let (text, bytes) = match m {
                                    Message::Text(s) => (true, s.as_bytes().to_vec()),
                                    Message::Binary(b) => (false, b.to_vec()),
                                    Message::Close(_) => break,
                                    _ => continue,
                                };
                                let frame = TunnelFrame::Data {
                                    client_id: writer_cid.clone(),
                                    text,
                                    payload_b64: URL_SAFE_NO_PAD.encode(&bytes),
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
                        payload_b64,
                    } => {
                        let bytes = match URL_SAFE_NO_PAD.decode(payload_b64.as_bytes()) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!("relay bad base64 payload: {e}");
                                continue;
                            }
                        };
                        let msg = if text {
                            match String::from_utf8(bytes) {
                                Ok(s) => Message::Text(s.into()),
                                Err(_) => continue,
                            }
                        } else {
                            Message::Binary(bytes.into())
                        };
                        if let Some(sess) = reader_sessions.lock().await.get(&client_id) {
                            let _ = sess.inbox_tx.send(msg);
                        }
                    }
                }
            }
        });

        // Writer: us → relay. One task that owns the sink, drains the
        // merged tunnel-frame queue (every per-phone-session pump funnels
        // here), and serializes onto the host WS as Text messages.
        let writer = tokio::spawn(async move {
            while let Some(frame) = host_out_rx.recv().await {
                let txt = match serde_json::to_string(&frame) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ws_sink.send(Message::Text(txt.into())).await.is_err() {
                    break;
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
