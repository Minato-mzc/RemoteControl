//! Library entry point — exposes all modules for the binary in `main.rs` and
//! for examples under `examples/`.

pub mod audio;
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
pub mod config;
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
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Boot the full server: discover IP, print/save QR, start WebSocket loop.
pub async fn run_server() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::load_or_default()?;
    let port = cfg.port;

    let addrs = net::discover_lan_ipv4()?;
    if addrs.is_empty() {
        anyhow::bail!("no usable private IPv4 found on this machine — check your network adapters");
    }

    info!("server name    = {}", cfg.server_name);
    info!("listening port = {}", port);
    info!("discovered {} candidate IPv4 address(es):", addrs.len());
    for a in &addrs {
        info!(
            "  - {:<15}  iface={:?}  kind={:?}",
            a.addr.to_string(),
            a.iface_name,
            a.kind
        );
    }
    let primary = &addrs[0];
    info!(
        "primary advertised = {} ({})",
        primary.addr, primary.iface_name
    );

    let pairing = pairing::PairingStore::new_with_fresh_code();
    let (code, key_b64) = pairing.current_qr_fields();
    qr::print_qr_to_terminal(&primary.addr.to_string(), port, &code, &key_b64)?;
    match qr::save_qr_html_and_open(&addrs, port, &code, &key_b64) {
        Ok(path) => info!("QR HTML written and opened: {}", path.display()),
        Err(e) => warn!("could not write QR HTML: {e:#}"),
    }

    // Open the persistent device-trust store. Failure to load is not fatal
    // (we just won't recognize previous reconnect tokens — first-time
    // pairing via QR still works) so we log + continue with an empty
    // store rather than aborting startup.
    let trusted = match trusted_devices::TrustedDevicesStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            warn!("trusted_devices store unavailable ({e:#}); starting with empty list");
            // Fall back to a temp store that still serves the runtime API
            // — verifies will all return UnknownDevice and the phone will
            // re-pair via QR.
            let tmp = std::env::temp_dir().join(format!(
                "remotecontrol_trusted_{}.json",
                uuid::Uuid::new_v4()
            ));
            trusted_devices::TrustedDevicesStore::open(tmp)?
        }
    };

    ws_server::run(primary.addr.to_string(), port, pairing, trusted, cfg).await
}
