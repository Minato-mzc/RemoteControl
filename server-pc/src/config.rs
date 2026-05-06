use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Version we advertise in the QR payload and accept as the canonical client version.
pub const PROTOCOL_VERSION: u32 = 6;
/// Lowest client `v` we still accept (M1 clients understand only handshake; we reject stream from them).
pub const MIN_SUPPORTED_VERSION: u32 = 1;
pub const PAIRING_CODE_TTL_SECS: u64 = 300; // 5 min
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    pub server_name: String,
    pub os: String,
}

impl Config {
    pub fn load_or_default() -> Result<Self> {
        let host = hostname_or("DESKTOP");
        Ok(Self {
            port: 7890,
            server_name: host,
            os: "Windows".to_string(),
        })
    }
}

fn hostname_or(fallback: &str) -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| fallback.to_string())
}
