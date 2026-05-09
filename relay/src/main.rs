//! RemoteControl cross-network relay.
//!
//! ## What it does
//! When the phone and PC aren't on the same LAN (e.g. the user is on
//! mobile data while the PC is at home), neither side can reach the
//! other directly because of NAT. This binary, deployed on a public
//! VPS the user owns, is the only thing both sides can both reach:
//! PC and phone each open an *outbound* WebSocket to the relay, and
//! the relay glues them together.
//!
//! ## Topology
//! ```text
//!                  POST /v1/host/register     (one-shot)
//!                  WS   /v1/host?…            (long-lived, PC waits here)
//!     PC server  ─────────────────────────►  relay  ◄─────────────────────────  Phone
//!                                                       WS /v1/client?host=…
//!                                                       (one per phone session)
//! ```
//! Once both ends are connected, the relay forwards bytes between
//! them. It speaks WebSocket on both sides so it can sit behind a
//! caddy/nginx Let's Encrypt termination.
//!
//! ## Tunnel framing (host side only)
//! The PC's long-lived WS multiplexes many phone sessions, so messages
//! on it are JSON envelopes:
//!   * `client_open  {client_id}`        — relay→host
//!   * `client_close {client_id}`        — relay→host
//!   * `data {client_id, text, payload_b64}` — bidirectional
//!
//! Phones' own WSes carry plain Text/Binary messages; the relay
//! base64-encodes/decodes the payloads when crossing into/out of the
//! tunnel. There's no envelope on the phone side — phones just see
//! exactly what the PC sends them, byte-identical to the LAN path.
//!
//! ## Auth
//! `host_id` is public (it goes in the QR). `host_token` is a 256-bit
//! secret minted by the relay at register time and kept on the PC.
//! The relay verifies SHA-256(token) before letting a host open its
//! long-lived WS. Phones aren't authenticated by the relay at all —
//! the existing M1 hello (HMAC over the QR pairing key) authenticates
//! them end-to-end through the tunnel, so the relay sees only opaque
//! bytes.
//!
//! ## TLS
//! Listen plain HTTP; deploy behind caddy/nginx for TLS:
//! ```caddyfile
//! relay.yourdomain.com {
//!     reverse_proxy localhost:7891
//! }
//! ```

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "RemoteControl cross-network relay")]
struct Args {
    /// Local TCP port to listen on. Put caddy/nginx in front for TLS.
    #[arg(long, default_value_t = 7891)]
    port: u16,

    /// Listen address. `0.0.0.0` binds all interfaces.
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let state = Arc::new(RelayState::default());

    let app = Router::new()
        .route("/v1/host/register", post(host_register))
        .route("/v1/host", get(host_ws))
        .route("/v1/client", get(client_ws))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let bind = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("relay listening on {bind} (front with caddy/nginx for TLS)");
    axum::serve(listener, app).await?;
    Ok(())
}

// ============================================================================
// Shared state
// ============================================================================

#[derive(Default)]
struct RelayState {
    /// Long-term host registry. Persists across host disconnects so the
    /// PC can come back and re-open the WS without re-registering. NOT
    /// persisted to disk in v1 — relay restart forces re-registration,
    /// which is cheap (PC keeps `relay.toml` and just calls register
    /// again on next launch). v2 can drop this to disk if it ever
    /// matters; for now an in-memory map is plenty.
    hosts: Mutex<HashMap<String, HostRecord>>,
    /// Live host bookkeeping. Set when the host's WS upgrades, removed
    /// when it drops. Phones look up their target here.
    online: Mutex<HashMap<String, OnlineHost>>,
}

#[derive(Clone)]
struct HostRecord {
    /// SHA-256 of the plaintext token. Same trick as the trusted_devices
    /// table on the PC — file leak alone doesn't grant access.
    token_hash: String,
    /// Display name from `register`. Useful for log lines.
    #[allow(dead_code)]
    name: String,
}

struct OnlineHost {
    /// Anything queued here ends up on the host's WebSocket as a
    /// `TunnelFrame`. The host writer task drains it.
    to_host: mpsc::UnboundedSender<TunnelFrame>,
    /// Per-client outbound queues. The host's reader pushes into the
    /// matching entry when it receives a `TunnelFrame::Data` aimed at
    /// `client_id`; the client_loop pumps that queue onto the phone's
    /// WS. Wrapped in a Mutex because the host loop and individual
    /// client loops both need to mutate the map (open/close on either
    /// side).
    clients: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<PhoneOut>>>>,
}

/// One message destined for a phone's WS.
struct PhoneOut {
    bytes: Vec<u8>,
    /// True → re-emit as `Message::Text`; false → `Message::Binary`.
    text: bool,
}

// ============================================================================
// Wire formats — binary tunnel framing
// ============================================================================
//
// Earlier iteration used JSON+base64. That added ~33% bandwidth and made
// every video frame an allocate+parse round-trip; over the cross-network
// path with limited home upstream bandwidth, the inflation reliably
// starved the tunnel and binary frames stopped arriving on the phone.
//
// New scheme: every host↔relay message is a Binary WebSocket frame with
// a fixed 38-byte header followed by an opaque payload.
//
//   offset  size  meaning
//   0       1     msg_type:  1=ClientOpen, 2=ClientClose, 3=Data
//   1       36    client_id: ASCII UUIDv4 string ("xxxxxxxx-...-xxxxxxxxxxxx")
//   37      1     text_flag: 1 if Data should be re-emitted as Message::Text
//                            (control plane JSON), 0 for Binary (video/audio).
//                            Reserved/zero for Open/Close.
//   38..    n     payload:   raw bytes of the original WS message (Data only)

#[derive(Debug)]
enum TunnelFrame {
    ClientOpen { client_id: String },
    ClientClose { client_id: String },
    Data {
        client_id: String,
        text: bool,
        payload: Vec<u8>,
    },
}

const TUNNEL_HEADER_LEN: usize = 38;
const TF_OPEN: u8 = 1;
const TF_CLOSE: u8 = 2;
const TF_DATA: u8 = 3;

impl TunnelFrame {
    fn encode(&self) -> Vec<u8> {
        match self {
            TunnelFrame::ClientOpen { client_id } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN);
                buf.push(TF_OPEN);
                push_uuid(&mut buf, client_id);
                buf.push(0); // text_flag unused
                buf
            }
            TunnelFrame::ClientClose { client_id } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN);
                buf.push(TF_CLOSE);
                push_uuid(&mut buf, client_id);
                buf.push(0);
                buf
            }
            TunnelFrame::Data {
                client_id,
                text,
                payload,
            } => {
                let mut buf = Vec::with_capacity(TUNNEL_HEADER_LEN + payload.len());
                buf.push(TF_DATA);
                push_uuid(&mut buf, client_id);
                buf.push(if *text { 1 } else { 0 });
                buf.extend_from_slice(payload);
                buf
            }
        }
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < TUNNEL_HEADER_LEN {
            return None;
        }
        let mty = bytes[0];
        let client_id = std::str::from_utf8(&bytes[1..37]).ok()?.to_string();
        let flag = bytes[37];
        match mty {
            TF_OPEN => Some(TunnelFrame::ClientOpen { client_id }),
            TF_CLOSE => Some(TunnelFrame::ClientClose { client_id }),
            TF_DATA => Some(TunnelFrame::Data {
                client_id,
                text: flag != 0,
                payload: bytes[TUNNEL_HEADER_LEN..].to_vec(),
            }),
            _ => None,
        }
    }
}

fn push_uuid(buf: &mut Vec<u8>, uuid: &str) {
    // UUIDs from `uuid::Uuid::new_v4().to_string()` are always 36 ASCII
    // chars in `xxxxxxxx-xxxx-Vxxx-xxxx-xxxxxxxxxxxx` form. If somehow
    // truncated/padded, push exactly 36 to keep the header layout stable.
    let mut bytes = [b'0'; 36];
    let src = uuid.as_bytes();
    let n = src.len().min(36);
    bytes[..n].copy_from_slice(&src[..n]);
    buf.extend_from_slice(&bytes);
}

// ============================================================================
// /v1/host/register
// ============================================================================

#[derive(Deserialize)]
struct RegisterReq {
    /// Optional human-readable PC name. Free-form; stored verbatim.
    #[serde(default)]
    name: String,
}

#[derive(Serialize)]
struct RegisterResp {
    host_id: String,
    host_token: String,
}

async fn host_register(
    State(state): State<Arc<RelayState>>,
    Json(req): Json<RegisterReq>,
) -> impl IntoResponse {
    use rand::rngs::OsRng;
    use rand::RngCore;
    // OsRng is `Send` (delegates to /dev/urandom or BCrypt) — important
    // because the future this produces is awaited across thread boundaries
    // by tokio. `rand::thread_rng()` would NOT compile here for that
    // reason: ThreadRng is `!Send` and can't survive an .await.
    let mut token_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut token_bytes);
    let host_token = base64_url_no_pad(&token_bytes);
    let host_id = uuid::Uuid::new_v4().to_string();
    let token_hash = sha256_hex(host_token.as_bytes());

    state.hosts.lock().await.insert(
        host_id.clone(),
        HostRecord {
            token_hash,
            name: req.name,
        },
    );
    info!("host registered  id={host_id}");
    Json(RegisterResp {
        host_id,
        host_token,
    })
}

// ============================================================================
// /v1/host (long-lived WS for the PC)
// ============================================================================

#[derive(Deserialize)]
struct HostWsParams {
    host_id: String,
    host_token: String,
}

async fn host_ws(
    State(state): State<Arc<RelayState>>,
    Query(p): Query<HostWsParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token_hash = sha256_hex(p.host_token.as_bytes());
    let allowed = matches!(
        state.hosts.lock().await.get(&p.host_id),
        Some(rec) if rec.token_hash == token_hash
    );
    if !allowed {
        return (StatusCode::UNAUTHORIZED, "bad host_id/host_token").into_response();
    }
    if state.online.lock().await.contains_key(&p.host_id) {
        // Only one PC instance per host_id — racy double-registration is
        // either a misconfiguration or a hostile takeover. Fail closed.
        return (StatusCode::CONFLICT, "host already online").into_response();
    }
    ws.on_upgrade(move |socket| host_loop(state, p.host_id, socket))
}

async fn host_loop(state: Arc<RelayState>, host_id: String, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let (to_host_tx, mut to_host_rx) = mpsc::unbounded_channel::<TunnelFrame>();
    let clients = Arc::new(Mutex::new(HashMap::<
        String,
        mpsc::UnboundedSender<PhoneOut>,
    >::new()));

    state.online.lock().await.insert(
        host_id.clone(),
        OnlineHost {
            to_host: to_host_tx.clone(),
            clients: clients.clone(),
        },
    );
    info!("host online  id={host_id}");

    // Writer: relay → host. Each TunnelFrame becomes a single Binary WS
    // frame on the wire. Binary instead of Text means video payloads (which
    // are themselves arbitrary binary) travel byte-for-byte without UTF-8
    // validation or base64 inflation.
    let writer = tokio::spawn(async move {
        while let Some(frame) = to_host_rx.recv().await {
            let bytes = frame.encode();
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Reader: host → relay → phone. Host always sends Binary tunnel frames.
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(_) => break,
        };
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            // Text from the host is unexpected with the binary protocol;
            // ignore so we don't loop on garbage.
            _ => continue,
        };
        let frame = match TunnelFrame::decode(&bytes) {
            Some(f) => f,
            None => {
                warn!("host {host_id}: malformed tunnel frame ({} bytes)", bytes.len());
                continue;
            }
        };
        match frame {
            TunnelFrame::Data {
                client_id,
                text,
                payload,
            } => {
                let map = clients.lock().await;
                if let Some(sender) = map.get(&client_id) {
                    let _ = sender.send(PhoneOut {
                        bytes: payload,
                        text,
                    });
                }
            }
            // Hosts don't initiate ClientOpen/Close — they just respond to
            // whatever phones the relay sends them.
            TunnelFrame::ClientOpen { .. } | TunnelFrame::ClientClose { .. } => {}
        }
    }

    // Drop OnlineHost so new phones bounce off /v1/client immediately.
    state.online.lock().await.remove(&host_id);
    // Closing this drops the outbound queue → writer task exits.
    drop(to_host_tx);
    let _ = writer.await;
    // Phones still attached have their per-client mpsc senders inside
    // `clients`; dropping the map closes them, so phone client_loops
    // exit on the next out_rx.recv().
    {
        let mut map = clients.lock().await;
        map.clear();
    }
    info!("host offline  id={host_id}");
}

// ============================================================================
// /v1/client (one WS per phone session)
// ============================================================================

#[derive(Deserialize)]
struct ClientWsParams {
    host: String,
}

async fn client_ws(
    State(state): State<Arc<RelayState>>,
    Query(p): Query<ClientWsParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !state.online.lock().await.contains_key(&p.host) {
        return (StatusCode::SERVICE_UNAVAILABLE, "host offline").into_response();
    }
    ws.on_upgrade(move |socket| client_loop(state, p.host, socket))
}

async fn client_loop(state: Arc<RelayState>, host_id: String, ws: WebSocket) {
    let client_id = uuid::Uuid::new_v4().to_string();
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<PhoneOut>();

    // Snapshot the host's bookkeeping refs. If the host went offline
    // between accept and now, abort cleanly.
    let (to_host, clients) = {
        let online = state.online.lock().await;
        match online.get(&host_id) {
            Some(h) => (h.to_host.clone(), h.clients.clone()),
            None => {
                warn!("client {client_id}: host {host_id} disappeared before tunnel setup");
                let _ = sink.close().await;
                return;
            }
        }
    };

    // Register the phone's outbound queue under client_id so the host's
    // reader can find it.
    clients
        .lock()
        .await
        .insert(client_id.clone(), out_tx.clone());

    if to_host
        .send(TunnelFrame::ClientOpen {
            client_id: client_id.clone(),
        })
        .is_err()
    {
        // Host's writer already exited → tunnel dead.
        clients.lock().await.remove(&client_id);
        let _ = sink.close().await;
        return;
    }
    info!("client connected  id={client_id}  host={host_id}");

    // Writer: host → phone
    let writer_client_id = client_id.clone();
    let writer_host_id = host_id.clone();
    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let msg = if out.text {
                match String::from_utf8(out.bytes) {
                    Ok(s) => Message::Text(s.into()),
                    Err(_) => continue, // host marked text but bytes weren't UTF-8 — drop
                }
            } else {
                Message::Binary(out.bytes.into())
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
        info!(
            "client writer exited  id={writer_client_id}  host={writer_host_id}"
        );
    });

    // Reader: phone → relay → host. Phone's WS messages (Text for
    // control plane JSON, Binary if the protocol ever needs upstream
    // binary) travel as TunnelFrame::Data — host re-emits them on its
    // virtual peer queue with the same kind.
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(_) => break,
        };
        let (text, bytes) = match msg {
            Message::Text(t) => (true, t.as_bytes().to_vec()),
            Message::Binary(b) => (false, b.to_vec()),
            Message::Close(_) => break,
            _ => continue,
        };
        if to_host
            .send(TunnelFrame::Data {
                client_id: client_id.clone(),
                text,
                payload: bytes,
            })
            .is_err()
        {
            break;
        }
    }

    // Cleanup: notify host, drop senders, await writer.
    let _ = to_host.send(TunnelFrame::ClientClose {
        client_id: client_id.clone(),
    });
    clients.lock().await.remove(&client_id);
    drop(out_tx);
    let _ = writer.await;
    info!("client disconnected  id={client_id}  host={host_id}");
}

// ============================================================================
// Helpers
// ============================================================================

fn sha256_hex(input: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(input);
    let bytes = h.finalize();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    Ok(URL_SAFE_NO_PAD.decode(s.as_bytes())?)
}
