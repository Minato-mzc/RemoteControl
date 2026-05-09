//! LAN-side WebSocket listener.
//!
//! Accepts inbound TCP, completes the WebSocket handshake, and bridges
//! the resulting frame stream to the transport-agnostic per-peer state
//! machine in [`crate::connection`]. The relay client uses the same
//! state machine but driven by tunneled frames instead — having both
//! transports share `connection::run_connection` is what made it cheap
//! to add cross-network support.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::Config;
use crate::connection::{run_connection, InboundRx, OutboundTx};
use crate::pairing::PairingStore;
use crate::trusted_devices::TrustedDevicesStore;

pub async fn run(
    host: String,
    port: u16,
    pairing: Arc<PairingStore>,
    trusted: Arc<TrustedDevicesStore>,
    cfg: Arc<Config>,
) -> Result<()> {
    let bind = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind).await?;
    info!("WebSocket listening on {bind} (advertised as {host}:{port})");

    loop {
        let (tcp, peer) = listener.accept().await?;
        let pairing = pairing.clone();
        let trusted = trusted.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge_lan_peer(tcp, peer, pairing, trusted, cfg).await {
                warn!("connection from {peer} ended: {e:#}");
            }
        });
    }
}

/// Set up the two `mpsc` channels (`InboundRx` going from the WS into
/// the state machine; `OutboundTx` going from the state machine back
/// onto the wire), spawn the WS pump, and run the state machine until
/// either side drops. This is the LAN counterpart to the relay client's
/// per-tunnel-session driver — both ultimately call `run_connection`.
async fn bridge_lan_peer(
    tcp: TcpStream,
    peer: std::net::SocketAddr,
    pairing: Arc<PairingStore>,
    trusted: Arc<TrustedDevicesStore>,
    cfg: Arc<Config>,
) -> Result<()> {
    let ws = accept_async(tcp).await?;
    let (mut sink, mut stream) = ws.split();

    // Unbounded channels are fine for the control plane (low volume,
    // bounded by phone's send rate). For the binary video frames, the
    // outbox can briefly hold a few hundred KB but the encoder side
    // already paces frame production, so backpressure isn't a concern
    // we'd see in practice.
    let (inbox_tx, inbox_rx): (
        mpsc::UnboundedSender<Message>,
        mpsc::UnboundedReceiver<Message>,
    ) = mpsc::unbounded_channel();
    let (outbox_tx, mut outbox_rx): (
        OutboundTx,
        mpsc::UnboundedReceiver<Message>,
    ) = mpsc::unbounded_channel();

    let peer_label = peer.to_string();

    // Two pump tasks bracket the protocol logic:
    //   reader: WS  → inbox_tx
    //   writer: outbox_rx → WS
    // Either dying tears down the other via channel close.
    let reader = {
        let inbox_tx = inbox_tx.clone();
        let label = peer_label.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(m) => {
                        if inbox_tx.send(m).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("ws read from {label}: {e}");
                        break;
                    }
                }
            }
        })
    };
    let writer = tokio::spawn(async move {
        while let Some(m) = outbox_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let logic_result = run_connection(
        peer_label,
        inbox_rx,
        outbox_tx,
        pairing,
        trusted,
        cfg,
    )
    .await;

    // Closing inbox_tx will let the reader's `inbox_tx.send` start failing,
    // and `outbox_rx.recv` returns None once the logic side drops outbox_tx.
    drop(inbox_tx);
    let _ = reader.await;
    let _ = writer.await;

    logic_result
}
