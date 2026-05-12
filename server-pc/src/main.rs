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
//!
//! ## Subsystem & logging
//! Release builds link with `windows_subsystem = "windows"` — Windows
//! doesn't allocate a console, so the user gets the tray icon as the
//! only UI (no flash of black cmd window on launch). Debug builds keep
//! the default console subsystem so `cargo run` still shows logs live.
//!
//! Logs always go to a daily-rolling file under
//! `%LOCALAPPDATA%\RemoteControl\logs\server.log.YYYY-MM-DD`. In release
//! we additionally try `AttachConsole(ATTACH_PARENT_PROCESS)` — if the
//! exe was launched from `cmd.exe` or PowerShell, logs tee back into
//! that terminal too. Panics are captured by a `set_hook` so even
//! crashes leave a record on disk.

// Release: GUI subsystem (no console window). Debug: default console
// subsystem so `cargo run` still prints to the terminal it was started
// from. cfg_attr is a no-op on non-Windows targets.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
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

    // Try to attach to the parent console so users who launched from a
    // terminal see logs there too. Silently no-op if there's no console
    // (Explorer double-click) or if a console is already attached
    // (debug builds — the console subsystem has one allocated). Always
    // compiled so `cargo check` exercises it; harmless in debug.
    #[cfg(windows)]
    attach_parent_console();

    // Set up tracing. Returns a WorkerGuard for the non-blocking file
    // appender — we bind to `_guard` here so it lives until `main`
    // returns. (Tray "Exit" calls `std::process::exit`, which bypasses
    // drop; that's a known minor flush-on-exit caveat.)
    let _guard = init_tracing()?;

    // Panic hook → tracing → log file. Without this, a panic on the
    // worker thread under GUI subsystem would disappear with no console
    // to print to.
    install_panic_hook();

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

/// Bring up the tracing subscriber with two writers stacked:
///   * a daily-rolling file under `%LOCALAPPDATA%\RemoteControl\logs\`,
///     so a release-build user has somewhere to look when something
///     goes wrong (GUI subsystem = no stderr to read);
///   * a stdout layer that is meaningful in debug builds (cargo run)
///     and in release builds that were launched from a terminal (where
///     `AttachConsole` ran above).
///
/// `RUST_LOG=…` still works as the env-filter override.
fn init_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = remotecontrol_server::paths::log_dir()?;
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create log dir {}", log_dir.display()))?;

    // `daily` rolls at midnight UTC and keeps every file ever written.
    // We don't currently prune old logs — diagnose-then-delete is fine
    // for self-hosted use and avoids any risk of nuking the wrong file.
    let appender = tracing_appender::rolling::daily(&log_dir, "server.log");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false);
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    tracing::info!("logs → {}", log_dir.display());
    Ok(guard)
}

/// Re-route panics through `tracing::error!` so they end up in the log
/// file. The default panic handler writes to stderr; under GUI subsystem
/// stderr has no console attached, which historically meant a crashed
/// release build vanished without trace.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("PANIC: {info}");
        // Chain to the default hook too — useful in debug builds where
        // it prints to stderr / the attached console.
        default(info);
    }));
}

/// `AttachConsole(ATTACH_PARENT_PROCESS)`. Silently fails when there's
/// no parent console (Explorer double-click, installer launch, …) or
/// when one is already attached (debug builds run under the console
/// subsystem) — both are expected and need no error handling.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
