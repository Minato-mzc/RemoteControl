//! Tiny embedded HTTP/1.1 server that serves the QR pairing page and
//! exposes a one-click "refresh code" endpoint.
//!
//! ## Why this exists
//! The pairing code has a 5-minute TTL. Previously, refreshing it meant
//! either (a) restarting the whole `remotecontrol-server` process or
//! (b) hitting Enter in the terminal. Both are annoying when the QR
//! page is already open in the browser. With this module the page itself
//! grows a "🔄 刷新二维码" button that hits `/refresh`, which rotates
//! the [`PairingStore`] and redirects back to `/`. The page rerenders
//! with the new code and the user keeps using the same browser tab.
//!
//! ## Why not add `axum`/`warp`
//! The existing tree pulls neither in directly (only the relay crate
//! does, and we don't want that cost on the PC binary). For two routes
//! and no body parsing, a hand-rolled HTTP/1.1 handler on raw
//! `tokio::net::TcpListener` is ~80 lines and avoids a 20+ crate dep
//! tree. Performance doesn't matter — this server services at most one
//! browser tab making a refresh click every couple minutes.

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{tcp::OwnedWriteHalf, TcpListener, TcpStream};
use tracing::{info, warn};

use crate::net::DiscoveredAddr;
use crate::pairing::PairingStore;
use crate::qr::{self, RelayQrInfo};
use crate::relay_client::RelayConfig;
use crate::ServerMode;

/// Shared inputs for re-rendering the QR HTML on each request. Cloned
/// behind an `Arc` so the per-connection task can read without holding
/// the listener loop.
pub struct QrServerArgs {
    pub bind_addr: String,
    pub pairing: Arc<PairingStore>,
    pub addrs: Vec<DiscoveredAddr>,
    pub port: u16,
    pub relay_cfg: Option<RelayConfig>,
    pub mode: ServerMode,
}

pub async fn run(args: QrServerArgs) -> Result<()> {
    let listener = TcpListener::bind(&args.bind_addr)
        .await
        .with_context(|| format!("bind qr_server {}", args.bind_addr))?;
    info!("QR HTTP server: open http://{} to view / refresh", args.bind_addr);
    let shared = Arc::new(args);
    loop {
        let (stream, _peer) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, shared).await {
                // One-line warn per failed connection — usually just a
                // browser closing the socket early on refresh.
                warn!("qr_server connection: {e:#}");
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, args: Arc<QrServerArgs>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);

    // Parse the request line. HTTP/1.1: METHOD SP PATH SP VERSION CRLF.
    let mut req_line = String::new();
    let n = read.read_line(&mut req_line).await?;
    if n == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = req_line.trim_end().split_whitespace().collect();
    if parts.len() < 3 {
        return Ok(());
    }
    let method = parts[0];
    let path = parts[1];

    // Drain headers — we don't need any of them for these routes.
    loop {
        let mut buf = String::new();
        let n = read.read_line(&mut buf).await?;
        if n == 0 || buf == "\r\n" || buf == "\n" {
            break;
        }
    }

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let html = render_qr_html(&args)?;
            send_html(&mut write, 200, "OK", &html).await
        }
        ("GET", "/refresh") | ("POST", "/refresh") => {
            args.pairing.rotate();
            let (code, _) = args.pairing.current_qr_fields();
            info!("QR refreshed via /refresh → new code={code}");
            // 303 See Other so the browser follows up with a GET / —
            // also makes "back button" behavior sensible: refreshing
            // the page after refresh doesn't re-rotate.
            send_response(
                &mut write,
                303,
                "See Other",
                Some(("Location", "/")),
                "refreshing…",
            )
            .await
        }
        ("GET", "/favicon.ico") => {
            send_response(&mut write, 404, "Not Found", None, "").await
        }
        _ => send_html(&mut write, 404, "Not Found", "<h1>404</h1>").await,
    }
}

fn render_qr_html(args: &QrServerArgs) -> Result<String> {
    let (code, key_b64) = args.pairing.current_qr_fields();
    let lan_addrs: &[DiscoveredAddr] = match args.mode {
        ServerMode::RelayOnly => &[],
        _ => &args.addrs,
    };
    let relay_info = args.relay_cfg.as_ref().map(|r| RelayQrInfo {
        base_url: r.base_url.as_str(),
        host_id: r.host_id.as_str(),
    });
    qr::build_qr_html(lan_addrs, args.port, &code, &key_b64, relay_info.as_ref())
}

async fn send_html(
    w: &mut OwnedWriteHalf,
    status: u16,
    status_text: &str,
    body: &str,
) -> Result<()> {
    send_response(w, status, status_text, None, body).await
}

async fn send_response(
    w: &mut OwnedWriteHalf,
    status: u16,
    status_text: &str,
    extra_header: Option<(&str, &str)>,
    body: &str,
) -> Result<()> {
    let body_bytes = body.as_bytes();
    let mut head = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n",
        body_bytes.len(),
    );
    if let Some((k, v)) = extra_header {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes()).await?;
    w.write_all(body_bytes).await?;
    w.flush().await?;
    Ok(())
}
