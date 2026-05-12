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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{tcp::OwnedWriteHalf, TcpListener, TcpStream};
use tracing::{info, warn};

use crate::file_send::{FileSendBridge, FileSendCmd};
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
    /// Shared registry connecting the drag-drop upload form on the QR
    /// page to whichever phone session is currently authenticated. The
    /// page POSTs raw file bytes to `/send-file` and we hand the
    /// spooled temp path off through this bridge.
    pub file_send_bridge: Arc<FileSendBridge>,
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
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Slurp headers into a tiny case-insensitive table. /send-file needs
    // Content-Length (body size) and X-File-Name (percent-encoded UTF-8
    // basename); the other routes ignore the table.
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut buf = String::new();
        let n = read.read_line(&mut buf).await?;
        if n == 0 || buf == "\r\n" || buf == "\n" {
            break;
        }
        if let Some(idx) = buf.find(':') {
            let key = buf[..idx].trim().to_ascii_lowercase();
            let val = buf[idx + 1..].trim().to_string();
            headers.push((key, val));
        }
    }

    match (method.as_str(), path.as_str()) {
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
        ("POST", "/send-file") => {
            handle_send_file(&mut read, &mut write, &headers, &args).await
        }
        ("GET", "/favicon.ico") => {
            send_response(&mut write, 404, "Not Found", None, "").await
        }
        _ => send_html(&mut write, 404, "Not Found", "<h1>404</h1>").await,
    }
}

/// `POST /send-file` — raw-body upload from the drag-drop form on the
/// QR page. Headers we care about:
///   * `Content-Length`: total bytes to read from the body
///   * `X-File-Name`: percent-encoded UTF-8 basename to display on the
///     phone and pick a destination filename from
///
/// We spool the body to a fresh file under the system temp dir and hand
/// it off via the [`FileSendBridge`]; the receiving `run_connection`
/// then drives the actual protocol exchange and deletes the spool when
/// it's done.
async fn handle_send_file(
    read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
    headers: &[(String, String)],
    args: &QrServerArgs,
) -> Result<()> {
    let get_header = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let Some(len_str) = get_header("content-length") else {
        return send_json(write, 400, "Bad Request", r#"{"ok":false,"reason":"missing content-length"}"#).await;
    };
    let len: u64 = match len_str.parse() {
        Ok(v) => v,
        Err(_) => {
            return send_json(write, 400, "Bad Request", r#"{"ok":false,"reason":"bad content-length"}"#).await;
        }
    };
    if len == 0 {
        return send_json(write, 400, "Bad Request", r#"{"ok":false,"reason":"empty file"}"#).await;
    }
    let display_name = get_header("x-file-name")
        .map(|s| percent_decode(s))
        .unwrap_or_else(|| "upload.bin".to_string());

    // Reject pre-flight if no session — saves spooling a multi-GB file
    // to disk only to discard it. Race-prone (the session can die
    // mid-upload), but cheap and a much better UX in the common case.
    if args.file_send_bridge.dispatch_dry_run().await.is_err() {
        return send_json(
            write,
            503,
            "Service Unavailable",
            r#"{"ok":false,"reason":"没有已连接的手机会话"}"#,
        ).await;
    }

    // Spool body to a fresh temp file. Don't use NamedTempFile here
    // because the consumer (run_connection) outlives this request and
    // will unlink the file itself once the transfer is done. Just give
    // it a uuid-tagged name so concurrent uploads from the browser
    // don't collide.
    let temp_dir = std::env::temp_dir();
    let temp_name = format!("rc-send-{}.bin", uuid::Uuid::new_v4());
    let temp_path = temp_dir.join(temp_name);
    {
        let mut f = tokio::fs::File::create(&temp_path)
            .await
            .with_context(|| format!("create temp spool {}", temp_path.display()))?;
        let mut remaining = len;
        let mut buf = vec![0u8; 256 * 1024];
        while remaining > 0 {
            let want = std::cmp::min(remaining as usize, buf.len());
            let n = read
                .read(&mut buf[..want])
                .await
                .with_context(|| "read body")?;
            if n == 0 {
                // EOF before Content-Length declared — bail and unlink.
                drop(f);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return send_json(
                    write,
                    400,
                    "Bad Request",
                    r#"{"ok":false,"reason":"client closed before body complete"}"#,
                )
                .await;
            }
            f.write_all(&buf[..n]).await.with_context(|| "write temp spool")?;
            remaining -= n as u64;
        }
        f.flush().await.with_context(|| "flush temp spool")?;
    }

    let cmd = FileSendCmd {
        name: display_name.clone(),
        size: len,
        temp_path: temp_path.clone(),
    };
    match args.file_send_bridge.dispatch(cmd).await {
        Ok(()) => {
            info!("send-file accepted: {} ({} bytes) → spool {}", display_name, len, temp_path.display());
            send_json(write, 200, "OK", r#"{"ok":true}"#).await
        }
        Err(reason) => {
            // Race: session went away between the dry-run and the
            // actual dispatch. Drop the spool and tell the user.
            let _ = tokio::fs::remove_file(&temp_path).await;
            let body = format!(r#"{{"ok":false,"reason":"{reason}"}}"#);
            send_json(write, 503, "Service Unavailable", &body).await
        }
    }
}

/// Minimal percent-decoder for `X-File-Name`. Only handles `%XX`
/// triplets — no `+` → space, since browsers don't form-encode header
/// values that way. Falls through to the raw byte on malformed
/// triplets so we don't lose data on weird filenames.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(h) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(h);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| {
        // Filename wasn't valid UTF-8 after decoding; fall back to a
        // safe lossy rendition. Modern phones display the result fine.
        String::from_utf8_lossy(&e.into_bytes()).into_owned()
    })
}

async fn send_json(
    w: &mut OwnedWriteHalf,
    status: u16,
    status_text: &str,
    body: &str,
) -> Result<()> {
    let body_bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body_bytes.len(),
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body_bytes).await?;
    w.flush().await?;
    Ok(())
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
