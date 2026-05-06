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
pub mod stream;
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

    ws_server::run(primary.addr.to_string(), port, pairing, cfg).await
}
