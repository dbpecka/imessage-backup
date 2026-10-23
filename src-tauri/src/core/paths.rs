use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Validate a user-supplied filesystem path destined for either a backup
/// export or a snapshot root. Rules:
/// - path must not be empty
/// - path must be absolute (Tauri's native file picker always produces
///   absolute paths; a relative path on the IPC wire is suspicious)
/// - path must resolve under the user's `$HOME` or onto a mounted volume
///   under `/Volumes/<name>/` (external drives are the natural place for an
///   archive)
/// - path must not be inside `~/Library/Messages`, where an export would
///   mix with live data (and look like orphaned attachments)
///
/// Returns the canonicalised path on success. Non-existent destinations are
/// permitted — we canonicalise the longest existing prefix and append the
/// remainder, so new subfolders can be created safely.
pub fn validate_user_path(raw: &str, purpose: &str) -> Result<PathBuf, AppError> {
    if raw.is_empty() {
        return Err(AppError::Other(format!("{purpose} is required")));
    }

    let p = PathBuf::from(raw);
    if !p.is_absolute() {
        return Err(AppError::Other(format!(
            "{purpose} must be an absolute path: {raw}"
        )));
    }

    let home = canonical_or_self(&crate::core::db_path::home_dir()?);
    let canonical = canonical_prefix(&p);

    let messages_dir = home.join("Library/Messages");
    if canonical.starts_with(&messages_dir) {
        return Err(AppError::Other(format!(
            "{purpose} can't be inside {}: {raw}",
            messages_dir.display()
        )));
    }

    if !canonical.starts_with(&home) && !is_on_mounted_volume(&canonical) {
        return Err(AppError::Other(format!(
            "{purpose} must be inside your home directory ({}) or on an external drive: {raw}",
            home.display()
        )));
    }

    Ok(canonical)
}

/// `true` for paths inside a volume mounted at `/Volumes/<name>`. The boot
/// volume's `/Volumes/Macintosh HD` entry is a symlink to `/`, so it
/// canonicalises away and is not accepted here.
fn is_on_mounted_volume(canonical: &Path) -> bool {
    canonical
        .strip_prefix("/Volumes")
        .map(|rest| rest.components().count() >= 1)
        .unwrap_or(false)
}

/// Canonicalise as much of `path` as already exists; preserve the rest.
/// Needed because `fs::canonicalize` errors when the target doesn't yet
/// exist (typical for new backup destinations).
pub(crate) fn canonical_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let parent = existing.parent().map(|p| p.to_path_buf());
        let name = existing.file_name().map(|n| n.to_os_string());
        match (parent, name) {
            (Some(parent), Some(name)) => {
                trailing.push(name);
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = canonical_or_self(&existing);
    for name in trailing.iter().rev() {
        resolved.push(name);
    }
    resolved
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        let err = validate_user_path("", "destination").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("required")));
    }

    #[test]
    fn rejects_relative() {
        let err = validate_user_path("./relative", "destination").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("absolute")));
    }

    #[test]
    fn rejects_outside_home() {
        // /tmp is outside $HOME on every supported platform.
        let err = validate_user_path("/tmp/../etc/passwd", "snapshot root").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("home directory")));
    }

    #[test]
    fn accepts_nonexistent_subpath_within_home() {
        let home = std::env::var("HOME").unwrap();
        let target = format!("{home}/.imessage-backup-test/nested/path");
        let resolved = validate_user_path(&target, "destination").unwrap();
        assert!(resolved.starts_with(&home));
        assert!(resolved.ends_with("nested/path"));
    }

    #[test]
    fn accepts_external_volume() {
        let resolved =
            validate_user_path("/Volumes/BubbleWrapTestDrive/archive", "destination").unwrap();
        assert!(resolved.ends_with("BubbleWrapTestDrive/archive"));
    }

    #[test]
    fn rejects_volumes_root() {
        let err = validate_user_path("/Volumes", "destination").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("external drive")));
    }

    #[test]
    fn rejects_messages_library() {
        let home = std::env::var("HOME").unwrap();
        let target = format!("{home}/Library/Messages/Attachments/export");
        let err = validate_user_path(&target, "destination").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("can't be inside")));
    }

    #[test]
    fn rejects_traversal_escaping_home() {
        let home = std::env::var("HOME").unwrap();
        let parent = Path::new(&home).parent().map(|p| p.to_path_buf()).unwrap();
        let target = format!("{}/not-a-home-neighbour-for-tests", parent.display());
        let err = validate_user_path(&target, "destination").unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("home directory")));
    }
}
