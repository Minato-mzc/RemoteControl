//! Library entry point — exposes all modules for the binary in `main.rs` and
//! for examples under `examples/`.

pub mod audio;
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
pub mod config;
pub mod connection;
pub mod encoder;
#[cfg(windows)]
pub mod input;
pub mod net;
pub mod pairing;
pub mod protocol;
pub mod qr;
pub mod relay_client;
pub mod stream;
pub mod trusted_devices;
pub mod video;
pub mod ws_server;

use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Which transports the binary should expose. Picked by `main.rs` from CLI
/// flags. Cross-network needs the relay to have been provisioned first
/// (via `--relay-register`); we surface that as a runtime error here
/// rather than re-running provisioning automatically — accidentally
/// minting a second host_id when the user wanted the existing one would
/// orphan the saved phone trust tokens.
#[derive(Debug, Clone, Copy)]
pub enum ServerMode {
    /// Only the LAN WebSocket listener. Default; works when the phone is
    /// on the same Wi-Fi as the PC.
    LanOnly,
    /// Both LAN listener and the outbound relay client. The phone can
    /// connect on whichever path is reachable from where it is.
    LanAndRelay,
    /// Only the outbound relay client; skip the LAN bind (rare — useful
    /// on a PC with no usable private IPv4, e.g. running entirely on
    /// virtual interfaces).
    RelayOnly,
}

/// Boot the full server. The exact transports started depend on `mode`.
pub async fn run_server(mode: ServerMode) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::load_or_default()?;
    let port = cfg.port;

    let addrs = net::discover_lan_ipv4()?;
    if matches!(mode, ServerMode::LanOnly | ServerMode::LanAndRelay) && addrs.is_empty() {
        anyhow::bail!("no usable private IPv4 found on this machine — check your network adapters");
    }

    info!("server name    = {}", cfg.server_name);
    info!("listening port = {}", port);
    info!("server mode    = {:?}", mode);
    info!("discovered {} candidate IPv4 address(es):", addrs.len());
    for a in &addrs {
        info!(
            "  - {:<15}  iface={:?}  kind={:?}",
            a.addr.to_string(),
            a.iface_name,
            a.kind
        );
    }

    // Pairing code is shared across both transports — the phone can
    // scan the LAN QR or the relay QR with the same code.
    let pairing = Arc::new(pairing::PairingStore::new_with_fresh_code());
    let (code, key_b64) = pairing.current_qr_fields();

    // Load or warn about the persistent trust store. Same fallback path
    // as before: failures are non-fatal because re-pairing via QR is
    // always available.
    let trusted = match trusted_devices::TrustedDevicesStore::open_default() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn!("trusted_devices store unavailable ({e:#}); starting with empty list");
            let tmp = std::env::temp_dir().join(format!(
                "remotecontrol_trusted_{}.json",
                uuid::Uuid::new_v4()
            ));
            Arc::new(trusted_devices::TrustedDevicesStore::open(tmp)?)
        }
    };

    // Optionally load relay config. Provisioning is one-shot and printed
    // by main.rs; here we just consume the saved file.
    let relay_cfg = if matches!(mode, ServerMode::LanAndRelay | ServerMode::RelayOnly) {
        match relay_client::load()? {
            Some(c) => Some(c),
            None => anyhow::bail!(
                "relay mode requested but no relay.toml exists — run \
                 `remotecontrol-server --relay-register https://your-relay.example.com` first"
            ),
        }
    } else {
        None
    };

    // Print the QR for whichever address(es) are advertised.
    if let Some(primary) = addrs.first() {
        info!(
            "primary advertised = {} ({})",
            primary.addr, primary.iface_name
        );
        if matches!(mode, ServerMode::LanOnly | ServerMode::LanAndRelay) {
            qr::print_qr_to_terminal(&primary.addr.to_string(), port, &code, &key_b64)?;
        }
    }
    if matches!(mode, ServerMode::LanOnly | ServerMode::LanAndRelay) {
        match qr::save_qr_html_and_open(&addrs, port, &code, &key_b64) {
            Ok(path) => info!("QR HTML written and opened: {}", path.display()),
            Err(e) => warn!("could not write QR HTML: {e:#}"),
        }
    }
    if let Some(rcfg) = &relay_cfg {
        // Relay-mode QR payload has its own scheme so the phone can
        // distinguish it from a LAN QR (different connection path).
        let relay_payload = format!(
            "rcrelay://{base}/?host={host}&v={v}&c={code}&k={key}",
            base = rcfg.base_url.trim_start_matches("https://").trim_start_matches("http://"),
            host = rcfg.host_id,
            v = config::PROTOCOL_VERSION,
            code = code,
            key = key_b64,
        );
        info!("relay QR payload  = {relay_payload}");
        // We don't currently render the relay QR into the HTML page —
        // adding it cleanly is part of the next polish pass. The user
        // can scan whichever code they prefer (LAN from the HTML, relay
        // from the log).
    }

    let cfg = Arc::new(cfg);
    let primary_str = addrs
        .first()
        .map(|a| a.addr.to_string())
        .unwrap_or_default();

    // Spawn whichever transports the mode requests. We `join!` them so
    // a failure in one tears the other down (rather than leaving the
    // user with a half-running server they think is working).
    let lan_fut = async {
        if matches!(mode, ServerMode::LanOnly | ServerMode::LanAndRelay) {
            ws_server::run(
                primary_str.clone(),
                port,
                pairing.clone(),
                trusted.clone(),
                cfg.clone(),
            )
            .await
        } else {
            futures_util::future::pending::<Result<()>>().await
        }
    };
    let relay_fut = async {
        if let Some(rcfg) = relay_cfg.clone() {
            relay_client::RelayClient::new(rcfg)
                .run(pairing.clone(), trusted.clone(), cfg.clone())
                .await
        } else {
            futures_util::future::pending::<Result<()>>().await
        }
    };
    tokio::select! {
        r = lan_fut => r,
        r = relay_fut => r,
    }
}
