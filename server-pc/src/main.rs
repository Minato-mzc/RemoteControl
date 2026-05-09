//! CLI entry point.
//!
//! Two modes of operation:
//!
//! ```text
//!   remotecontrol-server                                # default LAN mode
//!   remotecontrol-server --relay-register https://relay.example.com
//!                                                       # one-shot relay provisioning
//!   remotecontrol-server --relay                        # LAN + relay
//!   remotecontrol-server --relay-only                   # relay only (skip LAN listener)
//! ```

use anyhow::Result;
use clap::Parser;

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(base_url) = cli.relay_register {
        // Provisioning runs without the full server bootstrap — just enough
        // to know our config name. Subsequent launches re-read relay.toml.
        let cfg = remotecontrol_server::config::Config::load_or_default()?;
        let provisioned = remotecontrol_server::relay_client::provision(
            &base_url,
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
        return Ok(());
    }

    let mode = if cli.relay_only {
        remotecontrol_server::ServerMode::RelayOnly
    } else if cli.relay {
        remotecontrol_server::ServerMode::LanAndRelay
    } else {
        remotecontrol_server::ServerMode::LanOnly
    };
    remotecontrol_server::run_server(mode).await
}
