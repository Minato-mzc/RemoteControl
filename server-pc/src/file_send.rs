//! M6 v2 PC → phone file transfer plumbing.
//!
//! ## Roles
//! Two tasks need to talk: the QR HTTP server (where the user drops a
//! file in their browser) and the active connection's `run_connection`
//! loop (which owns the WebSocket the file ends up flowing through).
//! Neither knows the other directly — `run_server` wires them together
//! through a [`FileSendBridge`] that this module defines.
//!
//! ## Single active session
//! The PC can technically host multiple authenticated phone sessions at
//! once (LAN listener + relay client both spawn `run_connection`s). For
//! v1 we just target the most recent — each `run_connection` overwrites
//! the bridge slot on entry, and clears it on exit only if the slot still
//! identifies it (the relay's `next_instance` trick, applied here so a
//! stale session's exit can't take the live one's slot offline).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// Command from the QR HTTP server to the active connection. The HTTP
/// server has already spooled the upload to a temp file before sending
/// this; the connection task opens the temp file, streams its contents
/// to the phone as FILE chunks, and unlinks the temp file on completion
/// (success OR failure).
#[derive(Debug)]
pub struct FileSendCmd {
    /// Display name (basename only). Goes into `FileSendBegin.name` so
    /// the phone can show it and pick a destination filename.
    pub name: String,
    /// Total file size in bytes. The phone can pre-validate that it has
    /// enough free space before accepting.
    pub size: u64,
    /// Path to the on-disk spool file. Connection task is responsible
    /// for unlinking it once the transfer is done (or failed).
    pub temp_path: PathBuf,
}

/// Per-connection registration handle held by `run_connection`. Dropped
/// on its way out — but for the "still ours" check we compare instance
/// IDs (see [`FileSendBridge::deregister`]), not pointers, so a stale
/// `run_connection` that finally notices it should exit doesn't yank
/// the slot out from under its successor.
pub struct BridgeRegistration {
    instance: u64,
}

impl BridgeRegistration {
    pub fn instance(&self) -> u64 {
        self.instance
    }
}

/// Single-slot registry. Wrapped in `Arc` so the HTTP server and every
/// `run_connection` can share one instance.
pub struct FileSendBridge {
    inner: Mutex<Option<(u64, mpsc::UnboundedSender<FileSendCmd>)>>,
    next_instance: AtomicU64,
}

impl FileSendBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            next_instance: AtomicU64::new(0),
        })
    }

    /// Claim the slot. Returns a registration that the caller is expected
    /// to pass back to [`deregister`] on exit. If another session was
    /// already there, it's overwritten — its instance id won't match
    /// when *it* tries to deregister, so its later cleanup will be a
    /// no-op (correct: by then the new session owns the slot).
    pub async fn register(&self, tx: mpsc::UnboundedSender<FileSendCmd>) -> BridgeRegistration {
        let instance = self.next_instance.fetch_add(1, Ordering::Relaxed);
        *self.inner.lock().await = Some((instance, tx));
        BridgeRegistration { instance }
    }

    /// Release the slot — but only if the current occupant is still us.
    pub async fn deregister(&self, reg: BridgeRegistration) {
        let mut g = self.inner.lock().await;
        if let Some((cur, _)) = g.as_ref() {
            if *cur == reg.instance {
                *g = None;
            }
        }
    }

    /// Hand a command to whichever connection currently holds the slot.
    /// Returns Err with a human-readable reason if there's no session
    /// (so the HTTP layer can produce a sensible 503).
    pub async fn dispatch(&self, cmd: FileSendCmd) -> std::result::Result<(), &'static str> {
        let g = self.inner.lock().await;
        let (_, tx) = g.as_ref().ok_or("没有已连接的手机会话")?;
        tx.send(cmd).map_err(|_| "手机会话刚刚断开")
    }

    /// Probe — is there a session right now? Used by the HTTP upload
    /// handler to fail fast (with a 503) before spending time spooling
    /// a multi-GB body to disk only to discard it.
    pub async fn dispatch_dry_run(&self) -> std::result::Result<(), &'static str> {
        let g = self.inner.lock().await;
        if g.is_some() {
            Ok(())
        } else {
            Err("没有已连接的手机会话")
        }
    }
}
