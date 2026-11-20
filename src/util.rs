// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static START: OnceLock<Instant> = OnceLock::new();

/// Monotonic seconds since the process started.
pub fn now() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Wall-clock seconds since the Unix epoch.
pub fn wall() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The daemon's socket; JOY2CON_SOCKET overrides it.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("JOY2CON_SOCKET")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }));
    PathBuf::from(dir).join("joy2con.sock")
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("joy2con").join("config.json")
}

/// "AA:BB:..." with every byte as two hex digits.
pub fn mac_from_le(bytes: &[u8]) -> String {
    bytes.iter().rev().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}
