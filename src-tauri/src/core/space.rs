use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Free space (bytes) on the volume holding a path. Injectable so tests can
/// simulate a full disk.
pub type SpaceProbe = fn(&Path) -> std::io::Result<u64>;

/// The real probe: free space on the volume of `path`, or of its nearest
/// existing ancestor (backup and snapshot folders often don't exist yet).
pub fn system_available(path: &Path) -> std::io::Result<u64> {
    fs2::available_space(nearest_existing(path))
}

fn nearest_existing(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    while !p.exists() {
        match p.parent() {
            Some(parent) => p = parent.to_path_buf(),
            None => break,
        }
    }
    p
}

/// Extra room demanded beyond the estimate: 10%, at least 100 MB. Running a
/// volume completely full can break other apps (Messages included).
fn headroom(needed: u64) -> u64 {
    (needed / 10).max(100 * 1024 * 1024)
}

/// Refuse up front when `dir`'s volume can't hold `needed` bytes (plus
/// headroom), rather than failing halfway through a backup or snapshot.
pub fn ensure_space(
    dir: &Path,
    needed: u64,
    what: &str,
    probe: SpaceProbe,
) -> Result<(), AppError> {
    let available = probe(dir).map_err(|e| {
        AppError::Other(format!(
            "couldn't check free space for {what} at {}: {e}",
            dir.display()
        ))
    })?;
    let required = needed.saturating_add(headroom(needed));
    if available < required {
        return Err(AppError::Other(format!(
            "Not enough free space for {what}: about {} is needed at {} but only {} is free. \
             Free up space or choose another location.",
            human(required),
            dir.display(),
            human(available)
        )));
    }
    Ok(())
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Bytes a snapshot of `chat_db` will take: the database plus its
/// write-ahead log (the backup API copies both into one file).
pub fn snapshot_size(chat_db: &Path) -> u64 {
    let len = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut wal = chat_db.as_os_str().to_owned();
    wal.push("-wal");
    len(chat_db.to_path_buf()) + len(PathBuf::from(wal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_when_too_little_space_and_says_how_much() {
        let err = ensure_space(Path::new("/x"), 1024 * 1024 * 1024, "the backup", |_| {
            Ok(500 * 1024 * 1024)
        })
        .unwrap_err();
        assert!(
            matches!(&err, AppError::Other(m) if m.contains("1.1 GB") && m.contains("500.0 MB")),
            "{err:?}"
        );
    }

    #[test]
    fn allows_when_estimate_plus_headroom_fits() {
        assert!(ensure_space(Path::new("/x"), 10, "x", |_| Ok(200 * 1024 * 1024)).is_ok());
        // 100 MB headroom minimum.
        assert!(ensure_space(Path::new("/x"), 10, "x", |_| Ok(50 * 1024 * 1024)).is_err());
    }

    #[test]
    fn nearest_existing_walks_up_to_a_real_folder() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("a/b/c");
        assert_eq!(nearest_existing(&missing), tmp.path());
        assert!(system_available(&missing).unwrap() > 0);
    }
}
