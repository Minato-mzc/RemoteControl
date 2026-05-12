//! Per-user state directory layout.
//!
//! Everything the server writes between runs (relay credentials, trusted
//! device tokens, daily log files, …) lives under one well-known root:
//!
//! ```text
//!   Windows : %LOCALAPPDATA%\RemoteControl\
//!   *nix    : $XDG_CONFIG_HOME/RemoteControl/  (or $HOME/.config/RemoteControl/)
//! ```
//!
//! Centralising the lookup here means the "where do my logs / configs
//! live?" answer is one function call — also what the tray menu's
//! "open logs folder" action reaches for.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `LOCALAPPDATA\RemoteControl\` (or the *nix equivalent). Caller is
/// responsible for `create_dir_all` if it doesn't exist yet — we only
/// resolve the path so probes can stat it without side effects.
pub fn local_state_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
            .context("LOCALAPPDATA / APPDATA not set")?;
        Ok(base.join("RemoteControl"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(base.join("RemoteControl"))
    }
}

/// `…\RemoteControl\logs\`. The tracing-appender writes daily-rolling
/// files under this dir as `server.log.YYYY-MM-DD`.
pub fn log_dir() -> Result<PathBuf> {
    Ok(local_state_dir()?.join("logs"))
}
