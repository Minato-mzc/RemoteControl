use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::PAIRING_CODE_TTL_SECS;

/// One-shot pairing code + long-lived session key.
///
/// The pairing code authenticates the initial WS handshake and is consumed on
/// first successful use. The 32-byte key is delivered out-of-band via QR and
/// proves mutual possession (server signs client's nonce with HMAC-SHA256).
#[derive(Debug)]
struct Inner {
    code: String,
    key: [u8; 32],
    created: Instant,
    consumed: bool,
}

pub struct PairingStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    Ok,
    BadCode,
    Expired,
    Used,
}

impl PairingStore {
    pub fn new_with_fresh_code() -> Self {
        let mut rng = rand::thread_rng();
        let code: String = (0..6)
            .map(|_| rng.gen_range(0..10).to_string())
            .collect();
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        Self {
            inner: Mutex::new(Inner {
                code,
                key,
                created: Instant::now(),
                consumed: false,
            }),
        }
    }

    pub fn current_qr_fields(&self) -> (String, String) {
        let g = self.inner.lock().unwrap();
        (g.code.clone(), URL_SAFE_NO_PAD.encode(g.key))
    }

    pub fn key(&self) -> [u8; 32] {
        self.inner.lock().unwrap().key
    }

    /// Consume the code atomically: only the first valid attempt succeeds.
    pub fn verify_and_consume(&self, candidate: &str) -> VerifyResult {
        let mut g = self.inner.lock().unwrap();
        if g.consumed {
            return VerifyResult::Used;
        }
        if g.created.elapsed() > Duration::from_secs(PAIRING_CODE_TTL_SECS) {
            return VerifyResult::Expired;
        }
        if g.code != candidate {
            return VerifyResult::BadCode;
        }
        g.consumed = true;
        VerifyResult::Ok
    }

    /// Rotate to a new code (e.g. after a failed/expired session, or on user request).
    #[allow(dead_code)]
    pub fn rotate(&self) {
        let mut rng = rand::thread_rng();
        let mut g = self.inner.lock().unwrap();
        g.code = (0..6).map(|_| rng.gen_range(0..10).to_string()).collect();
        rng.fill(&mut g.key);
        g.created = Instant::now();
        g.consumed = false;
    }
}
