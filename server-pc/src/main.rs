//! CLI entry point.
//!
//! ## Modes
//! ```text
//!   remotecontrol-server                                # default LAN mode
//!   remotecontrol-server --relay-register https://relay.example.com
//!                                                       # one-shot relay provisioning
//!   remotecontrol-server --relay                        # LAN + relay
//!   remotecontrol-server --relay-only                   # relay only (skip LAN listener)
//! ```
//!
//! ## Threading layout
//! Server modes run two threads:
//!   * **Main thread** — owns the system tray icon and pumps Win32
//!     messages so menu clicks are delivered. Blocks in
//!     `tray::run_tray_loop` until the user picks Exit.
//!   * **Worker thread** — owns a tokio multi-thread runtime and
//!     drives `run_server`. Doing the tokio runtime on a worker rather
//!     than the main thread is needed because `tray-icon` requires
//!     its event loop on the main thread on Windows.
//!
//! One-shot `--relay-register` skips the tray entirely (no long-lived
//! server, just an HTTP call) and runs the tokio runtime on main.

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "RemoteControl PC server")]
struct Cli {
    /// One-shot: register this PC with the relay at the given URL,
    /// save host_id/host_token to %LOCALAPPDATA%/RemoteControl/relay.toml,
    /// then exit. Subsequent launches with `--relay` use the saved credentials.
    #[arg(long, value_name = "BASE_URL")]
    relay_register: Option<String>,

    /// Run the relay client alongside the LAN listener. Requires the PC
    /// to have already been provisioned (see --relay-register).
    #[arg(long, default_value_t = false)]
    relay: bool,

    /// Run only the relay client, skipping the LAN WebSocket listener.
    /// Useful when the PC has no usable LAN address (rare).
    #[arg(long, default_value_t = false)]
    relay_only: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Init tracing first so both the tray loop and the server worker
    // share the same subscriber.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Relay-register is a one-shot HTTP call; no tray, no worker.
    if let Some(base_url) = cli.relay_register {
        return run_relay_register(&base_url);
    }

    let mode = if cli.relay_only {
        remotecontrol_server::ServerMode::RelayOnly
    } else if cli.relay {
        remotecontrol_server::ServerMode::LanAndRelay
    } else {
        remotecontrol_server::ServerMode::LanOnly
    };

    // Build shared bits on the main thread so we can hand them to both
    // the tray (for the "refresh QR" action) and the server worker
    // (which actually drives the protocol).
    let cfg = remotecontrol_server::config::Config::load_or_default()?;
    let qr_http_port = cfg.port.saturating_add(1);
    let pairing = Arc::new(
        remotecontrol_server::pairing::PairingStore::new_with_fresh_code(),
    );
    let tray_state =
        remotecontrol_server::tray::TrayState::new(pairing.clone(), qr_http_port);

    // Server worker: owns the tokio runtime, drives all I/O. Detached
    // — when the tray exits, `std::process::exit` brings everything
    // down with it.
    let server_pairing = pairing.clone();
    let server_tray = tray_state.clone();
    std::thread::Builder::new()
        .name("server-runtime".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            if let Err(e) = rt.block_on(remotecontrol_server::run_server(
                mode,
                server_pairing,
                Some(server_tray),
            )) {
                tracing::error!("server exited with error: {e:#}");
            }
        })?;

    // Main thread blocks in the tray event loop. tray-icon and tao's
    // Win32 message pump both need the main thread on Windows.
    remotecontrol_server::tray::run_tray_loop(tray_state)
}

#[tokio::main(flavor = "current_thread")]
async fn run_relay_register(base_url: &str) -> Result<()> {
    let cfg = remotecontrol_server::config::Config::load_or_default()?;
    let provisioned = remotecontrol_server::relay_client::provision(
        base_url,
        &cfg.server_name,
    )
    .await?;
    println!("\nRelay provisioning complete.");
    println!("  base_url   = {}", provisioned.base_url);
    println!("  host_id    = {}", provisioned.host_id);
    println!(
        "  host_token = (saved to %LOCALAPPDATA%/RemoteControl/relay.toml; \
         do not share)"
    );
    println!("\nNext launch: `remotecontrol-server --relay` to enable cross-network mode.");
    Ok(())
}
