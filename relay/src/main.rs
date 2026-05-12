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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{error, info, warn};
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

    /// Path to the JSON file used to persist registered hosts across
    /// relay restarts. Previously the registry lived in memory only and
    /// `systemctl restart` forced every PC to re-run `--relay-register`
    /// (and every phone to re-scan the new QR). With this file the
    /// registry survives restarts — the relay loads it on boot and
    /// rewrites it atomically after each `host_register`.
    ///
    /// Atomic write = `tempfile in same dir → fsync → rename`, so a
    /// crash mid-write leaves either the old or the new copy intact,
    /// never a half-written one.
    #[arg(long, default_value = "relay-hosts.json")]
    hosts_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let hosts_file = PathBuf::from(&args.hosts_file);
    let loaded = load_hosts_from_disk(&hosts_file).await;
    info!(
        "loaded {} host(s) from {}",
        loaded.len(),
        hosts_file.display()
    );
    let state = Arc::new(RelayState {
        hosts: Mutex::new(loaded),
        online: Mutex::new(HashMap::new()),
        next_instance: AtomicU64::new(0),
        hosts_file,
    });

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

/// Read the on-disk hosts registry. Missing file is fine (cold start);
/// any other error (permission denied, malformed JSON) is logged and we
/// fall back to an empty map so the relay can still serve fresh
/// registrations. We deliberately don't panic — a corrupted hosts file
/// shouldn't take the whole relay down for everyone.
async fn load_hosts_from_disk(path: &Path) -> HashMap<String, HostRecord> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("hosts file {} not present — starting empty", path.display());
            return HashMap::new();
        }
        Err(e) => {
            error!("read {} failed: {} — starting empty", path.display(), e);
            return HashMap::new();
        }
    };
    match serde_json::from_slice::<HashMap<String, HostRecord>>(&bytes) {
        Ok(m) => m,
        Err(e) => {
            error!(
                "parse {} failed: {} — starting empty",
                path.display(),
                e
            );
            HashMap::new()
        }
    }
}

/// Atomic write: dump to a sibling temp file, fsync, rename onto the
/// target. If we crash mid-write the rename hasn't happened, so the
/// existing copy survives. Errors are logged but don't propagate —
/// losing a single persist is annoying (it'll redo itself on the next
/// register) but should not refuse the register itself, since the
/// in-memory copy is the live truth either way.
async fn persist_hosts_to_disk(path: &Path, snapshot: &HashMap<String, HostRecord>) {
    let json = match serde_json::to_vec_pretty(snapshot) {
        Ok(b) => b,
        Err(e) => {
            error!("serialize hosts failed: {e}");
            return;
        }
    };
    // Same-dir temp file so the rename stays on one filesystem; cross-
    // device rename would fail with EXDEV.
    let tmp = match path.parent() {
        Some(dir) => dir.join(format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("hosts.json")
        )),
        None => PathBuf::from(".hosts.json.tmp"),
    };
    if let Err(e) = tokio::fs::write(&tmp, &json).await {
        error!("write {} failed: {e}", tmp.display());
        return;
    }
    // Best-effort fsync of the temp file. If it fails we still attempt
    // the rename — worst case is a crash window where the file isn't
    // fully on disk yet, no different from the old in-memory regime.
    if let Ok(f) = tokio::fs::OpenOptions::new().read(true).open(&tmp).await {
        let _ = f.sync_all().await;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        error!("rename {} -> {} failed: {e}", tmp.display(), path.display());
    }
}

// ============================================================================
// Shared state
// ============================================================================

struct RelayState {
    /// Long-term host registry. Persisted to `hosts_file` so `systemctl
    /// restart` doesn't force every PC to re-run `--relay-register`.
    /// In-memory copy is the live truth; disk is updated atomically
    /// (write-tmp-then-rename) after each `host_register`.
    hosts: Mutex<HashMap<String, HostRecord>>,
    /// Live host bookkeeping. Set when the host's WS upgrades, removed
    /// when it drops. Phones look up their target here. NOT persisted —
    /// "currently online" is a runtime fact, not a registration fact.
    online: Mutex<HashMap<String, OnlineHost>>,
    /// Monotonic counter handed out per host_loop on entry. Lets the
    /// loop notice "the entry in `online` is no longer mine" so it
    /// doesn't clobber a successor when its own stale TCP finally dies.
    /// Without this, a PC that's already been displaced would, on its
    /// reader exit, `online.remove(host_id)` and silently take down the
    /// new live session.
    next_instance: AtomicU64,
    /// Where to write the hosts registry. Populated from `--hosts-file`.
    hosts_file: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
struct HostRecord {
    /// SHA-256 of the plaintext token. Same trick as the trusted_devices
    /// table on the PC — file leak alone doesn't grant access.
    token_hash: String,
    /// Display name from `register`. Useful for log lines.
    #[allow(dead_code)]
    name: String,
}

struct OnlineHost {
    /// Per-loop instance tag. Compared against the local `my_instance`
    /// at cleanup time — if they don't match, the entry already belongs
    /// to a successor (we were displaced) and we leave it alone.
    instance: u64,
    /// Anything queued here ends up on the host's WebSocket as a
    /// `TunnelFrame`. The host writer task drains it.
    ///
    /// Bounded — capacity [`TO_HOST_QUEUE_CAP`]. The unbounded predecessor
    /// let one misbehaving phone (e.g. a stalled 1GB+ upload) accumulate
    /// gigabytes of stale frames here while the PC drained at line speed,
    /// stranding every later phone's `ClientOpen` behind that pile for
    /// minutes. With a bound, congestion propagates back into the offending
    /// `client_loop`'s `.send().await`, which propagates back to the
    /// phone's TCP, which trips the phone-side `waitForBufferRoom` throttle
    /// — i.e. one phone slows itself down without affecting peers.
    to_host: mpsc::Sender<TunnelFrame>,
    /// Per-client outbound queues. Two channels per phone:
    ///   * `fast` — VIDEO / AUDIO frames. Droppable at insertion (a
    ///     stale frame helps no one; phone's decoder will recover
    ///     within one keyframe).
    ///   * `bulk` — text (helloOk, authOk, …) and FILE chunks. Must
    ///     deliver in order; insertion blocks the host_loop reader
    ///     until there's room.
    ///
    /// Splitting them in two prevents the failure we hit in production:
    /// a multi-MiB PC→phone file send filled the per-client queue with
    /// must-deliver FILE chunks, and every subsequent VIDEO frame got
    /// dropped on `try_send` — i.e. the user got a frozen screen on
    /// the phone while a file was downloading. With separate queues,
    /// the writer can keep emitting fresh VIDEO frames even while the
    /// FILE pipeline is back-pressured against the phone's downlink.
    clients: Arc<Mutex<HashMap<String, ClientChannels>>>,
    /// Notified by a *new* host_loop when it displaces this entry.
    /// The old reader selects on this in addition to its WS stream so
    /// we don't have to wait for TCP keepalive (default 2hr on Linux)
    /// to surface a dead peer — the new connection is the authoritative
    /// signal that the old one is stale.
    cancel: Arc<Notify>,
}

/// The two per-client senders the host_loop reader picks between based
/// on the WS frame type carried in `payload[0]`.
#[derive(Clone)]
struct ClientChannels {
    fast: mpsc::Sender<PhoneOut>,
    bulk: mpsc::Sender<PhoneOut>,
}

/// One message destined for a phone's WS.
struct PhoneOut {
    bytes: Vec<u8>,
    /// True → re-emit as `Message::Text`; false → `Message::Binary`.
    text: bool,
}

/// Per-phone "fast" queue capacity (VIDEO / AUDIO — droppable). At
/// 30 fps that's ~270 ms of buffered video. Sized deliberately small
/// to bound the **stale-frame latency** seen after a bulk send pause:
/// every `sink.send(FILE_chunk)` on the bulk arm commits the writer
/// for the chunk's transmission time (200 ms – 1 s on real links).
/// While that's in flight, new video frames pile up in `fast`. With
/// a 64-frame cap (the original tuning) the user would see ~2 s of
/// catch-up playback after each chunk; at 8 it's a quarter second,
/// which reads as a brief blur instead of "video is 2 s behind."
///
/// The cost is dropping more frames during sustained bulk transfers,
/// but with our 1 s keyframe interval any drop is recovered inside a
/// second — not noticeable for typical screen-share content.
const PER_CLIENT_FAST_CAP: usize = 8;

/// Per-phone "bulk" queue capacity (text control plane + FILE chunks
/// — must-deliver, blocking send). 32 × 256 KiB = 8 MiB max in flight,
/// which the host_loop reader uses to apply back-pressure all the way
/// to the PC sender when the phone's downlink can't keep up. Sized to
/// match `TO_HOST_QUEUE_CAP` for symmetry — beyond this, the host
/// reader blocks in `.send().await`, which propagates upstream so the
/// PC's outbound queue doesn't accumulate gigabytes either.
const PER_CLIENT_BULK_CAP: usize = 32;

/// Shared phone→PC queue capacity. All phone sessions on a given host
/// share this one channel (the host has a single inbound WS, so we have
/// to serialize anyway). Sized to absorb a brief PC stall — e.g. the
/// PC's WS sink momentarily stops draining — without forcing the relay
/// to drop control-plane traffic.
///
/// At up to 256 KB per file-chunk frame, 32 slots is ≤8 MiB of
/// buffering. Two constraints fix the upper bound:
///   * A new phone's `ClientOpen` has to ride at the back of this FIFO
///     before the PC sees it. Bigger queue → bigger reconnect latency
///     under congestion. 8 MiB drains in <1s on a LAN and ~7s on a
///     10 Mbps PC downlink — both acceptable.
///   * Phone-side OkHttp ping timeout is 20s. If the relay's reader
///     blocks for >20s waiting on `to_host` capacity (because the PC
///     is slow), the phone will tear down its WS as a keepalive
///     failure. 8 MiB / 1 Mbps ≈ 64s would be too risky; 8 MiB at the
///     slowest realistic PC downlink (10 Mbps) stays comfortably under
///     20s.
///
/// Past 32 slots, the phone-side `client_loop` blocks in
/// `.send().await`, which is the back-pressure signal we want: each
/// phone slows itself down independently, instead of one phone
/// exhausting RAM for everybody.
const TO_HOST_QUEUE_CAP: usize = 32;

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

    // Snapshot the registry under the lock so we can release it before
    // touching the filesystem — disk I/O shouldn't hold the mutex against
    // concurrent register/host_ws callers.
    let snapshot = {
        let mut hosts = state.hosts.lock().await;
        hosts.insert(
            host_id.clone(),
            HostRecord {
                token_hash,
                name: req.name,
            },
        );
        hosts.clone()
    };
    persist_hosts_to_disk(&state.hosts_file, &snapshot).await;
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
    // No 409 anymore. If something is already live on this host_id, we
    // are by definition the same owner (we just passed token check), so
    // we get to take the slot. The takeover happens inside `host_loop`
    // under the `online` lock so the displacement is atomic with the
    // insert of our own entry — phones that race in during the swap
    // either find the old entry (and route to the doomed loop, which
    // is harmless: it'll close shortly) or the new one.
    ws.on_upgrade(move |socket| host_loop(state, p.host_id, socket))
}

async fn host_loop(state: Arc<RelayState>, host_id: String, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let (to_host_tx, mut to_host_rx) =
        mpsc::channel::<TunnelFrame>(TO_HOST_QUEUE_CAP);
    let clients = Arc::new(Mutex::new(HashMap::<String, ClientChannels>::new()));

    let my_instance = state.next_instance.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(Notify::new());

    // Atomic swap-in. If someone was already there (a stale loop whose
    // TCP hasn't surfaced as dead yet), we displace it: dropping `old`
    // here drops its `to_host` sender clone, but the old loop also holds
    // its own clone, so we additionally `notify_one()` the old loop's
    // cancel signal to wake its select! and break the reader.
    let displaced = {
        let mut online = state.online.lock().await;
        online.insert(
            host_id.clone(),
            OnlineHost {
                instance: my_instance,
                to_host: to_host_tx.clone(),
                clients: clients.clone(),
                cancel: cancel.clone(),
            },
        )
    };
    if let Some(old) = displaced {
        warn!(
            "host {host_id}: displacing stale session  old_instance={}",
            old.instance
        );
        old.cancel.notify_one();
        // Drain phones attached to the old session so they reconnect
        // through the new tunnel rather than sitting on a dead route.
        old.clients.lock().await.clear();
    }
    info!("host online  id={host_id}  instance={my_instance}");

    // Writer: relay → host. Each TunnelFrame becomes a single Binary WS
    // frame on the wire. Binary instead of Text means video payloads (which
    // are themselves arbitrary binary) travel byte-for-byte without UTF-8
    // validation or base64 inflation.
    //
    // Each `sink.send` is bounded at 15 s. This pairs with the bounded
    // `to_host` channel above: if the PC's TCP buffer is genuinely wedged
    // (router crash, kernel buffer full, NIC reset), we'd otherwise spin
    // here forever holding the channel hostage. 15 s matches the per-
    // client writer's timeout; on a healthy link any single send finishes
    // in tens of milliseconds.
    let writer_host_id = host_id.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = to_host_rx.recv().await {
            let bytes = frame.encode();
            match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                sink.send(Message::Binary(bytes.into())),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("host {writer_host_id}: send error: {e}");
                    break;
                }
                Err(_) => {
                    warn!(
                        "host {writer_host_id}: send timed out after 15s \
                         (PC link wedged), disconnecting"
                    );
                    break;
                }
            }
        }
        let _ = sink.close().await;
    });

    // Reader: host → relay → phone. Host always sends Binary tunnel frames.
    // Selects on the cancel signal too so a successor can boot us out
    // immediately instead of waiting on TCP keepalive.
    loop {
        tokio::select! {
            _ = cancel.notified() => {
                warn!("host {host_id}  instance={my_instance}: cancelled by successor");
                break;
            }
            maybe = stream.next() => {
                let Some(item) = maybe else { break };
                let msg = match item {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let bytes = match msg {
                    Message::Binary(b) => b,
                    Message::Close(_) => break,
                    // Text from the host is unexpected with the binary
                    // protocol; ignore so we don't loop on garbage.
                    _ => continue,
                };
                let frame = match TunnelFrame::decode(&bytes) {
                    Some(f) => f,
                    None => {
                        warn!(
                            "host {host_id}: malformed tunnel frame ({} bytes)",
                            bytes.len()
                        );
                        continue;
                    }
                };
                match frame {
                    TunnelFrame::Data {
                        client_id,
                        text,
                        payload,
                    } => {
                        // Snapshot the channels out of the map so we
                        // don't hold the mutex across `.await` — that'd
                        // serialize all client routing through one lock.
                        let chans = {
                            let map = clients.lock().await;
                            map.get(&client_id).cloned()
                        };
                        if let Some(chans) = chans {
                            // Look at the protocol-level frame_type byte
                            // (offset 0 of the payload, see PROTOCOL.md
                            // §"数据平面") so we can pick the right
                            // delivery policy. Values must stay in sync
                            // with `server-pc/src/protocol.rs::frame_type`
                            // and `app-android/.../Protocol.kt::FrameType`.
                            //   0x01 VIDEO — droppable (decoder recovers
                            //                at next IDR, ~1 s).
                            //   0x02 AUDIO — droppable (lost samples are
                            //                preferable to lagged audio).
                            //   0x03 FILE  — MUST NOT drop; loss corrupts
                            //                the receiver's reassembled
                            //                output.
                            //
                            // The bulk queue (text + FILE) and the fast
                            // queue (VIDEO/AUDIO) are independent, so
                            // even a long-running file send doesn't
                            // crowd VIDEO frames out — the writer task
                            // selects from both and prefers fast, so
                            // the phone keeps getting fresh frames
                            // through the entire transfer.
                            const FRAME_TYPE_FILE: u8 = 0x03;
                            let frame_type =
                                payload.first().copied().unwrap_or(0);
                            let must_deliver =
                                text || frame_type == FRAME_TYPE_FILE;
                            let out = PhoneOut { bytes: payload, text };
                            if must_deliver {
                                // Control-plane JSON or FILE chunks →
                                // bulk channel. Blocking send applies
                                // back-pressure to the host writer if
                                // the phone's downlink is the bottleneck.
                                let _ = chans.bulk.send(out).await;
                            } else {
                                // Video / audio frame → fast channel.
                                // Drop on full: a stale A/V frame helps
                                // no one. Phone's decoder glitches
                                // until the next IDR and recovers,
                                // which is far better than playing back
                                // many seconds of content that arrived
                                // after the user already moved on.
                                if chans.fast.try_send(out).is_err() {
                                    // QueueFull or QueueClosed; neither
                                    // is worth a per-frame log (would
                                    // flood under load). Map cleanup
                                    // handles the Closed case.
                                }
                            }
                        }
                    }
                    // Hosts don't initiate ClientOpen/Close — they just
                    // respond to whatever phones the relay sends them.
                    TunnelFrame::ClientOpen { .. } | TunnelFrame::ClientClose { .. } => {}
                }
            }
        }
    }

    // Cleanup. CRITICAL: only remove our own entry. If we were displaced
    // (cancel fired), the entry in `online` belongs to the successor and
    // removing it would silently take the live session offline.
    {
        let mut online = state.online.lock().await;
        let still_ours = online
            .get(&host_id)
            .map(|h| h.instance == my_instance)
            .unwrap_or(false);
        if still_ours {
            online.remove(&host_id);
        }
    }
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
    info!("host offline  id={host_id}  instance={my_instance}");
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
    // Two queues per client; see `ClientChannels` for the policy split.
    let (fast_tx, mut fast_rx) =
        mpsc::channel::<PhoneOut>(PER_CLIENT_FAST_CAP);
    let (bulk_tx, mut bulk_rx) =
        mpsc::channel::<PhoneOut>(PER_CLIENT_BULK_CAP);

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

    // Register the phone's outbound queues under client_id so the host's
    // reader can route into them.
    clients.lock().await.insert(
        client_id.clone(),
        ClientChannels {
            fast: fast_tx.clone(),
            bulk: bulk_tx.clone(),
        },
    );

    if to_host
        .send(TunnelFrame::ClientOpen {
            client_id: client_id.clone(),
        })
        .await
        .is_err()
    {
        // Host's writer already exited → tunnel dead.
        clients.lock().await.remove(&client_id);
        let _ = sink.close().await;
        return;
    }
    info!("client connected  id={client_id}  host={host_id}");

    // Pong channel — phone Pings come in via the reader, get reflected
    // through this onto the writer task. axum/tokio-tungstenite does NOT
    // auto-respond to Ping, and OkHttp on the phone gives up after
    // `pingInterval` (20s by default) without a Pong. So we DIY the
    // keepalive bounce-back here.
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Writer: host → phone (and phone-Pong-replies). One task that owns
    // `sink`, draining three sources:
    //   1. pong_rx — phone-Ping replies (highest priority, biased first)
    //   2. fast_rx — VIDEO/AUDIO (next priority — keep the screen alive)
    //   3. bulk_rx — text control plane + FILE chunks (last; bulk
    //                transfers shouldn't starve interactive video)
    //
    // `biased;` polls arms in declaration order. Combined with the
    // separate queues, this means a long-running file send pushes
    // chunks at the same rate as before, but every time a fresh video
    // frame lands the writer dispatches it first — so the user keeps
    // seeing the PC's screen during a download.
    let writer_client_id = client_id.clone();
    let writer_host_id = host_id.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                maybe_pong = pong_rx.recv() => {
                    // `None` = the reader dropped `pong_tx` during cleanup
                    // (phone disconnect). We MUST break here, not continue:
                    // `biased` polls pong first every iteration, so a closed
                    // pong_rx makes the select hit this arm every time with
                    // no awaiting, and `continue` turns into a tight CPU
                    // spin that never lets `let _ = writer.await` in the
                    // cleanup path return. The visible symptom is exactly
                    // the bug we hit in production: the relay's client_loop
                    // never reaches its "client disconnected" log line, the
                    // phone keeps showing pings forever, and the PC never
                    // gets the ClientClose so its run_connection holds the
                    // stream (and DXGI) hostage for every subsequent peer.
                    let Some(payload) = maybe_pong else { break };
                    let plen = payload.len();
                    if let Err(e) = sink.send(Message::Pong(payload.into())).await {
                        warn!("client {writer_client_id}: pong send failed: {e}");
                        break;
                    }
                    info!("client {writer_client_id}: pong sent {plen} bytes");
                }
                maybe_out = fast_rx.recv() => {
                    let Some(out) = maybe_out else { break };
                    if !send_to_phone(&mut sink, out, &writer_client_id).await {
                        break;
                    }
                }
                maybe_out = bulk_rx.recv() => {
                    let Some(out) = maybe_out else { break };
                    if !send_to_phone(&mut sink, out, &writer_client_id).await {
                        break;
                    }
                }
                else => break,
            }
        }
        let _ = sink.close().await;
        info!(
            "client writer exited  id={writer_client_id}  host={writer_host_id}"
        );
    });
    // Reader: phone → relay → host.
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(_) => break,
        };
        let (text, bytes) = match msg {
            Message::Text(t) => (true, t.as_bytes().to_vec()),
            Message::Binary(b) => (false, b.to_vec()),
            Message::Close(_) => break,
            Message::Ping(p) => {
                info!("client {client_id}: ping {} bytes -> queueing pong", p.len());
                let _ = pong_tx.send(p.to_vec());
                continue;
            }
            Message::Pong(_) => continue,
            other => {
                warn!("client {client_id}: unexpected message variant {:?}", other);
                continue;
            }
        };
        if to_host
            .send(TunnelFrame::Data {
                client_id: client_id.clone(),
                text,
                payload: bytes,
            })
            .await
            .is_err()
        {
            break;
        }
    }
    // Reader exited → drop pong_tx so writer's pong_rx returns None.
    drop(pong_tx);

    // Cleanup: notify host, drop senders, await writer.
    let _ = to_host
        .send(TunnelFrame::ClientClose {
            client_id: client_id.clone(),
        })
        .await;
    clients.lock().await.remove(&client_id);
    // Dropping both senders closes both queues, which makes the writer's
    // `recv` returns flip to `None` and the select-`else => break` arm
    // fires. That lets `writer.await` actually return.
    drop(fast_tx);
    drop(bulk_tx);
    let _ = writer.await;
    info!("client disconnected  id={client_id}  host={host_id}");
}

/// Send one `PhoneOut` to the phone's WS sink with a 15 s per-frame
/// timeout. Returns `true` if the send succeeded (caller continues),
/// `false` if it failed or timed out (caller breaks the writer loop).
///
/// Cap rationale: with `PER_CLIENT_FAST_CAP` upstream dropping surplus
/// VIDEO frames and `PER_CLIENT_BULK_CAP` back-pressuring everything
/// else, this timeout is purely a "kernel TCP send buffer is wedged"
/// detector. A healthy 4G session, even with brief radio dips, finishes
/// any single send well under 5 s. 15 s is below the phone's old 20 s
/// OkHttp pong-timeout (we'd already bumped phone's `pingInterval` to
/// 60 s, but staying under any reasonable client-side keepalive means
/// when we DO tear down, the phone sees a clean WS close instead of a
/// confusing "didn't receive pong" error). Generous enough that
/// ordinary bufferbloat from a brief tower handover doesn't kill the
/// session.
async fn send_to_phone(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    out: PhoneOut,
    writer_client_id: &str,
) -> bool {
    let msg = if out.text {
        match String::from_utf8(out.bytes) {
            Ok(s) => Message::Text(s.into()),
            Err(_) => return true, // skip non-utf8 "text" payloads silently
        }
    } else {
        Message::Binary(out.bytes.into())
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        sink.send(msg),
    )
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            warn!("client {writer_client_id}: send error: {e}");
            false
        }
        Err(_) => {
            warn!(
                "client {writer_client_id}: send timed out after 15s \
                 (link wedged), disconnecting"
            );
            false
        }
    }
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
