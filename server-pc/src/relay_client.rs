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
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::warn;

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
// Virtual peer — message channels exposed to the existing ws_server logic so
// the relay tunnel and a real WebSocket are interchangeable transports.
// ============================================================================

/// One half of a [`VirtualPeer`] pair: messages from the phone arrive on
/// `incoming`; messages going to the phone get pushed onto `outgoing`.
/// The relay client task drives `outgoing` onto its host WebSocket as
/// `TunnelFrame::Data`s, and feeds `incoming` from the matching tunnel
/// frames. The existing `handle_connection_inner` (to be extracted from
/// `ws_server::handle_connection`) reads from `incoming` and writes to
/// `outgoing` — totally agnostic to whether the peer is a real WS or
/// the relay.
pub struct VirtualPeer {
    /// Messages flowing phone → PC.
    pub incoming: mpsc::Receiver<TunnelMessage>,
    /// Messages flowing PC → phone.
    pub outgoing: mpsc::Sender<TunnelMessage>,
}

/// Wire-equivalent of a single WebSocket message inside the tunnel.
#[derive(Debug, Clone)]
pub enum TunnelMessage {
    Text(String),
    Binary(Vec<u8>),
}

#[allow(dead_code)] // used as TODO scaffolding, actual run() lands in next change
pub struct RelayClient {
    cfg: RelayConfig,
}

impl RelayClient {
    pub fn new(cfg: RelayConfig) -> Self {
        Self { cfg }
    }

    /// Run the long-lived host WebSocket loop. Returns when the WS
    /// drops; caller should reconnect with backoff.
    ///
    /// **Stub** — the wiring into ws_server is the next milestone. For
    /// now we just log if asked to run.
    pub async fn run(self) -> Result<()> {
        warn!(
            "RelayClient.run() called but the integration with ws_server is not finished yet — \
             relay mode is currently no-op (host_id={})",
            self.cfg.host_id
        );
        Ok(())
    }
}
