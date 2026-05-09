//! Long-lived device trust store.
//!
//! After a phone successfully pairs via QR, we mint a 256-bit random
//! "trust token" and persist `device_id → SHA-256(token)` in
//! `%LOCALAPPDATA%\RemoteControl\trusted_devices.json`. The phone keeps
//! its copy of the plaintext token; on reconnect it sends the token back
//! and we look up the hash here. The phone never has to scan a QR again
//! unless this file is deleted or the entry is revoked.
//!
//! Only the *hash* is stored — same threat model as a password file. A
//! reader of `trusted_devices.json` can't impersonate the device.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrustedDevice {
    /// Server-assigned stable id (UUID v4). Echoed back by the phone in
    /// `trusted_hello.device_id` so we don't have to scan the whole list
    /// every reconnect.
    pub device_id: String,
    /// Hex-encoded SHA-256 of the plaintext token. The phone has the
    /// plaintext; we only ever check `sha256(input) == this`.
    pub token_hash: String,
    /// Display name from the phone's `ClientInfo.name` (e.g. "PLR-AL30").
    /// Used for the eventual "trusted devices" UI on the PC and for log
    /// messages.
    pub device_name: String,
    /// Unix-millis timestamp of when this device first paired. Useful
    /// for sorting / for the user to know which entry is which.
    pub created_unix_ms: u64,
    /// Last successful reconnect, for "stale device" pruning later.
    pub last_seen_unix_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustedDevicesFile {
    /// Schema version — bump when the layout changes so old files become
    /// "ignore + start fresh" rather than corrupting deserialization.
    #[serde(default = "default_schema_version")]
    schema: u32,
    #[serde(default)]
    devices: Vec<TrustedDevice>,
}

fn default_schema_version() -> u32 {
    1
}

const CURRENT_SCHEMA: u32 = 1;

/// In-memory cache + on-disk persistence. All reads/writes go through the
/// `Mutex` so the WS server (multi-threaded) can safely mutate during
/// concurrent connection attempts.
pub struct TrustedDevicesStore {
    path: PathBuf,
    inner: Mutex<TrustedDevicesFile>,
}

impl TrustedDevicesStore {
    /// Open / create the store at the platform-appropriate path
    /// (`%LOCALAPPDATA%\RemoteControl\trusted_devices.json` on Windows).
    /// Missing or unreadable file → start with an empty list (we don't
    /// abort — the user can always re-pair).
    pub fn open_default() -> Result<Self> {
        let path = default_path()?;
        Self::open(path)
    }

    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let inner = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<TrustedDevicesFile>(&bytes) {
                Ok(f) if f.schema == CURRENT_SCHEMA => f,
                Ok(_) => {
                    tracing::warn!(
                        "trusted_devices.json schema mismatch, starting fresh: {}",
                        path.display()
                    );
                    TrustedDevicesFile::default()
                }
                Err(e) => {
                    tracing::warn!(
                        "trusted_devices.json parse error ({e}), starting fresh: {}",
                        path.display()
                    );
                    TrustedDevicesFile::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustedDevicesFile::default(),
            Err(e) => {
                tracing::warn!("trusted_devices.json read error ({e}), starting fresh");
                TrustedDevicesFile::default()
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(TrustedDevicesFile {
                schema: CURRENT_SCHEMA,
                devices: inner.devices,
            }),
        })
    }

    /// Mint a new device entry: generates a 256-bit token and a UUID-v4
    /// device id, persists `(device_id, sha256(token))`, and returns the
    /// pair so the WS server can hand it back to the phone in `welcome`.
    /// The plaintext token is returned but never stored.
    pub fn mint(&self, device_name: String) -> Result<(String, String)> {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut token_bytes = [0u8; 32];
        rng.fill_bytes(&mut token_bytes);
        // Base64-url without padding so the token is safe in URLs and JSON.
        let token = base64_url_no_pad(&token_bytes);
        let device_id = uuid::Uuid::new_v4().to_string();
        let token_hash = sha256_hex(token.as_bytes());
        let now_ms = unix_ms_now();
        let dev = TrustedDevice {
            device_id: device_id.clone(),
            token_hash,
            device_name,
            created_unix_ms: now_ms,
            last_seen_unix_ms: now_ms,
        };
        {
            let mut f = self.inner.lock().expect("trusted devices mutex poisoned");
            f.devices.push(dev);
        }
        self.save()?;
        Ok((device_id, token))
    }

    /// Verify a `(device_id, token)` pair from a `trusted_hello`. Returns
    /// `Ok(true)` and updates `last_seen` on a match, `Ok(false)` on
    /// no-match (the WS server will reject with `BadTrustToken` /
    /// `UnknownDevice`).
    pub fn verify(&self, device_id: &str, token: &str) -> Result<VerifyOutcome> {
        let token_hash = sha256_hex(token.as_bytes());
        let mut f = self.inner.lock().expect("trusted devices mutex poisoned");
        let dev = match f.devices.iter_mut().find(|d| d.device_id == device_id) {
            Some(d) => d,
            None => return Ok(VerifyOutcome::UnknownDevice),
        };
        if dev.token_hash != token_hash {
            return Ok(VerifyOutcome::BadToken);
        }
        dev.last_seen_unix_ms = unix_ms_now();
        let device_name = dev.device_name.clone();
        // Drop the lock before saving — save grabs it again.
        drop(f);
        // Persist the last_seen update. Failure is non-fatal: the in-memory
        // state has the new timestamp, and the disk copy will catch up the
        // next time we save.
        if let Err(e) = self.save() {
            tracing::warn!("trusted_devices.json save (last_seen) failed: {e:#}");
        }
        Ok(VerifyOutcome::Ok { device_name })
    }

    fn save(&self) -> Result<()> {
        let f = self.inner.lock().expect("trusted devices mutex poisoned");
        let bytes = serde_json::to_vec_pretty(&*f)?;
        // Atomic write: write to a sibling temp file then rename. Avoids
        // half-written JSON if the process is killed mid-save.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} → {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum VerifyOutcome {
    Ok { device_name: String },
    UnknownDevice,
    BadToken,
}

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

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(target_os = "windows")]
fn default_path() -> Result<PathBuf> {
    let local_appdata = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .context("LOCALAPPDATA / APPDATA not set")?;
    Ok(local_appdata
        .join("RemoteControl")
        .join("trusted_devices.json"))
}

#[cfg(not(target_os = "windows"))]
fn default_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(base.join("RemoteControl").join("trusted_devices.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mint_and_verify_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trusted.json");
        let store = TrustedDevicesStore::open(path.clone()).unwrap();

        let (id, token) = store.mint("PLR-AL30".into()).unwrap();
        assert!(matches!(
            store.verify(&id, &token).unwrap(),
            VerifyOutcome::Ok { device_name } if device_name == "PLR-AL30"
        ));
        assert!(matches!(
            store.verify(&id, "wrong-token").unwrap(),
            VerifyOutcome::BadToken
        ));
        assert!(matches!(
            store.verify("missing-id", &token).unwrap(),
            VerifyOutcome::UnknownDevice
        ));

        // Reload from disk: still recognized.
        let store2 = TrustedDevicesStore::open(path).unwrap();
        assert!(matches!(
            store2.verify(&id, &token).unwrap(),
            VerifyOutcome::Ok { .. }
        ));
    }
}
