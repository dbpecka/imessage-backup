use std::process::Command;

use crate::error::AppError;

/// Best-effort detection of whether "Messages in iCloud" is enabled.
///
/// There is no single authoritative API — Apple exposes the setting through
/// several preference files depending on macOS version. We try the most
/// reliable signals in order and return `None` when nothing matches so the
/// UI can warn neutrally ("we couldn't confirm — please verify yourself")
/// rather than silently assuming one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ICloudState {
    Enabled,
    Disabled,
    Unknown,
}

pub fn detect_icloud_messages() -> Result<ICloudState, AppError> {
    // Signal 1: com.apple.Messages → ICloudSync (observed on Sonoma/Sequoia).
    if let Some(v) = read_defaults("com.apple.Messages", "ICloudSync") {
        if let Some(b) = parse_bool(&v) {
            return Ok(if b {
                ICloudState::Enabled
            } else {
                ICloudState::Disabled
            });
        }
    }

    // Signal 2: com.apple.madrid → SyncConfig (older naming).
    if let Some(v) = read_defaults("com.apple.madrid", "kSyncDisabled") {
        if let Some(b) = parse_bool(&v) {
            // kSyncDisabled → invert.
            return Ok(if b {
                ICloudState::Disabled
            } else {
                ICloudState::Enabled
            });
        }
    }

    Ok(ICloudState::Unknown)
}

fn read_defaults(domain: &str, key: &str) -> Option<String> {
    let out = Command::new("/usr/bin/defaults")
        .args(["read", domain, key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "true" | "YES" | "yes" => Some(true),
        "0" | "false" | "NO" | "no" => Some(false),
        _ => None,
    }
}
