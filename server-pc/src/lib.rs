//! Library entry point — exposes all modules for the binary in `main.rs` and
//! for examples under `examples/`.

pub mod audio;
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
pub mod config;
pub mod connection;
pub mod encoder;
pub mod file_send;
pub mod tray;
#[cfg(windows)]
pub mod input;
pub mod net;
pub mod pairing;
pub mod protocol;
pub mod qr;
pub mod qr_server;
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
///
/// `pairing` is created by `main.rs` and shared with the tray event
/// loop (so the "refresh QR" menu item can rotate it directly).
/// `tray_state` is `None` only on test paths that don't bring up the
/// tray; production always passes `Some`.
pub async fn run_server(
    mode: ServerMode,
    pairing: Arc<pairing::PairingStore>,
    tray_state: Option<Arc<tray::TrayState>>,
) -> Result<()> {
    // Tracing init is now done in `main.rs` before the tray thread
    // starts, so log output captures both server and tray events
    // from process start. Calling it twice would panic — skip here.

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

    // `pairing` is now created by main.rs so the tray loop can hold a
    // clone of it for the "refresh QR" menu action. The fresh code on
    // process start is still produced via `PairingStore::new_with_fresh_code`
    // in main; this function just consumes it.
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
    let relay_qr_info = relay_cfg.as_ref().map(|r| qr::RelayQrInfo {
        base_url: r.base_url.as_str(),
        host_id: r.host_id.as_str(),
    });
    // Spin up the local QR HTTP server (port = WS port + 1 by default)
    // and open the browser there. The page contains a "🔄 刷新二维码"
    // button that hits the server's `/refresh` endpoint to rotate the
    // pairing code without restarting the process — nice when the
    // 5-minute TTL lapses mid-setup.
    let render_html = matches!(
        mode,
        ServerMode::LanOnly | ServerMode::LanAndRelay | ServerMode::RelayOnly
    );
    let qr_http_port = port.saturating_add(1);
    if render_html {
        // Suppress the unused-warning when both `code` and `key_b64`
        // are not needed here anymore (the QR HTTP server reads fresh
        // values from `pairing` on each request).
        let _ = (&code, &key_b64);
        info!(
            "QR page available at http://127.0.0.1:{qr_http_port}/ \
             (tray opens an embedded WebView on first launch)"
        );
    }
    if let Some(rcfg) = &relay_cfg {
        let authority = rcfg
            .base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        info!(
            "relay QR payload  = rcrelay://{authority}/?host={host}&v={v}&c={code}&k=…",
            host = rcfg.host_id,
            v = config::PROTOCOL_VERSION,
            code = code,
        );
    }

    let cfg = Arc::new(cfg);
    let primary_str = addrs
        .first()
        .map(|a| a.addr.to_string())
        .unwrap_or_default();

    // Shared single-slot registry connecting the QR HTTP server (which
    // accepts drag-and-drop uploads from the browser) to the active
    // phone session's `run_connection`. Each `run_connection` writes its
    // command sender into this on entry and clears it on exit; the HTTP
    // server queries the slot when a file lands. See `file_send` for
    // the staleness-safe register/deregister handshake.
    let file_send_bridge = file_send::FileSendBridge::new();

    // Tray-state peer counter, cloned out of `tray_state` so both
    // transports can update it without holding the rest of the state.
    let peer_count_clone = tray_state.as_ref().map(|s| s.peer_count.clone());

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
                file_send_bridge.clone(),
                peer_count_clone.clone(),
            )
            .await
        } else {
            futures_util::future::pending::<Result<()>>().await
        }
    };
    let relay_fut = async {
        let Some(rcfg) = relay_cfg.clone() else {
            return futures_util::future::pending::<Result<()>>().await;
        };
        // Reconnect loop. `RelayClient::run` is one shot — it returns
        // when the underlying WS dies (clean close, network failure,
        // application-level Ping timeout, etc.). We want the user's
        // PC to keep itself dialable across home network blips, the
        // relay being restarted, and middlebox-induced silent TCP
        // drops, so we loop with exponential backoff (capped at 30 s)
        // and just keep trying forever. The select! above never
        // resolves on this branch in normal operation — only LAN
        // failure or qr-server failure tears the whole server down.
        let mut backoff_secs = 1u64;
        loop {
            let start = std::time::Instant::now();
            match relay_client::RelayClient::new(rcfg.clone())
                .run(
                    pairing.clone(),
                    trusted.clone(),
                    cfg.clone(),
                    file_send_bridge.clone(),
                    peer_count_clone.clone(),
                )
                .await
            {
                Ok(()) => {
                    info!("relay tunnel closed cleanly — reconnecting");
                }
                Err(e) => {
                    warn!("relay tunnel error: {e:#} — reconnecting");
                }
            }
            // Reset backoff if we stayed connected for a while.
            // A flapping connection that dies in <5 s shouldn't ramp
            // straight back to 1 s retries (that's a tight loop), but
            // a tunnel that lived for hours and then died once
            // shouldn't pay a 30 s penalty either.
            if start.elapsed() >= std::time::Duration::from_secs(60) {
                backoff_secs = 1;
            }
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    };
    let qr_http_fut = async {
        if render_html {
            qr_server::run(qr_server::QrServerArgs {
                bind_addr: format!("127.0.0.1:{qr_http_port}"),
                pairing: pairing.clone(),
                addrs: addrs.clone(),
                port,
                relay_cfg: relay_cfg.clone(),
                mode,
                file_send_bridge: file_send_bridge.clone(),
            })
            .await
        } else {
            futures_util::future::pending::<Result<()>>().await
        }
    };
    tokio::select! {
        r = lan_fut => r,
        r = relay_fut => r,
        r = qr_http_fut => r,
    }
}

#[cfg(target_os = "windows")]
fn open_in_default_browser(url: &str) {
    // `cmd /C start "" <url>` opens the URL in the system default
    // browser. Empty quoted arg is a CMD quirk: without it, `start`
    // treats the next quoted token as a window title rather than as the
    // target to open.
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

#[cfg(not(target_os = "windows"))]
fn open_in_default_browser(_url: &str) {
    // Not currently wired on non-Windows builds; user can navigate
    // there manually if needed.
}
