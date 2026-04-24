use std::process::Command;

use crate::error::AppError;

/// Returns `true` if macOS's `Messages.app` process is currently running.
///
/// Uses `pgrep -x Messages` — exact-match on the process name so we don't
/// flag "MessagesAgent" or unrelated apps. An absent binary is treated as
/// "not running" rather than an error.
pub fn is_messages_running() -> Result<bool, AppError> {
    let out = match Command::new("/usr/bin/pgrep")
        .args(["-x", "Messages"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return Err(AppError::Other(format!(
                "unable to invoke pgrep: {e}. Messages.app running status is unknown."
            )));
        }
    };

    // pgrep exits 0 if a match is found, 1 if not, other codes on error.
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(code) => Err(AppError::Other(format!(
            "pgrep returned exit code {code}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
        None => Err(AppError::Other(
            "pgrep terminated without an exit code".into(),
        )),
    }
}
