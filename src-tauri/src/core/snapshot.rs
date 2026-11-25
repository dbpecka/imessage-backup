use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

use rusqlite::{backup::Backup, Connection, OpenFlags};

use crate::error::AppError;

/// Result of snapshotting `chat.db` to a safety folder before a destructive
/// operation. The caller should surface `dir` in the UI so the user knows
/// where the snapshot landed and how to roll back if needed.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Snapshot {
    pub dir: PathBuf,
    /// Paths actually written. Populated for logging / future display.
    pub copied: Vec<PathBuf>,
}

/// Snapshot `chat.db` atomically into a timestamped folder under
/// `snapshot_root` using SQLite's online backup API. The backup holds a
/// shared lock on the source, so the resulting single-file `chat.db` is a
/// consistent point-in-time image regardless of WAL state — no need to
/// copy `-wal`/`-shm` sidecars or to pre-checkpoint.
pub fn snapshot_chat_db(chat_db: &Path, snapshot_root: &Path) -> Result<Snapshot, AppError> {
    if !chat_db.exists() {
        return Err(AppError::Other(format!(
            "chat.db not found at {}",
            chat_db.display()
        )));
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let dir = create_unique_dir(snapshot_root, &timestamp)?;

    let db_dest = dir.join("chat.db");

    let src = Connection::open_with_flags(
        chat_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        AppError::Other(format!(
            "failed to open {} for snapshot: {e}",
            chat_db.display()
        ))
    })?;

    let mut dst = Connection::open(&db_dest).map_err(|e| {
        AppError::Other(format!(
            "failed to create snapshot at {}: {e}",
            db_dest.display()
        ))
    })?;

    {
        let backup = Backup::new(&src, &mut dst)
            .map_err(|e| AppError::Other(format!("failed to init online backup: {e}")))?;
        backup
            .run_to_completion(1024, std::time::Duration::from_millis(0), None)
            .map_err(|e| AppError::Other(format!("online backup failed: {e}")))?;
    }

    drop(src);
    drop(dst);

    Ok(Snapshot {
        dir,
        copied: vec![db_dest],
    })
}

/// Create a fresh `root/<base>` directory, appending `-1`, `-2`, … if it
/// already exists. Two snapshots in the same second must never share a
/// folder, or the second would overwrite the only pre-delete copy taken by
/// the first.
pub fn create_unique_dir(root: &Path, base: &str) -> Result<PathBuf, AppError> {
    fs::create_dir_all(root)?;
    let mut dir = root.join(base);
    let mut n = 1u32;
    loop {
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => {
                dir = root.join(format!("{base}-{n}"));
                n += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` against `chat_db`. Retained as a
/// defensive pre-step so readers of the *source* database find a clean WAL;
/// the snapshot itself no longer depends on it (we use the SQLite backup
/// API, which is WAL-safe).
pub fn checkpoint_wal(chat_db: &Path) -> Result<(), AppError> {
    let conn = crate::core::db_path::open_write(chat_db)
        .map_err(|e| AppError::Other(format!("failed to open for checkpoint: {e}")))?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .map_err(|e| AppError::Other(format!("wal_checkpoint failed: {e}")))?;
    Ok(())
}

/// What a snapshot was taken for. Each kind gets its own folder and is
/// pruned on its own, so routine orphan cleanups can never push out the
/// snapshot taken before a message delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotKind {
    Delete,
    OrphanClean,
    /// The live database just before a restore, so a restore can be undone.
    PreRestore,
}

impl SnapshotKind {
    pub const ALL: [SnapshotKind; 3] = [
        SnapshotKind::Delete,
        SnapshotKind::OrphanClean,
        SnapshotKind::PreRestore,
    ];

    pub fn dir_name(self) -> &'static str {
        match self {
            SnapshotKind::Delete => "delete",
            SnapshotKind::OrphanClean => "orphan-clean",
            SnapshotKind::PreRestore => "pre-restore",
        }
    }
}

/// Default snapshot root: `<app data dir>/snapshots/<kind>`, i.e.
/// `~/Library/Application Support/io.pecka.apps.bubblewrap/snapshots/delete`.
/// Deliberately not under `~/Documents`, which "Desktop & Documents in
/// iCloud" uploads to iCloud Drive and may evict from local disk.
pub fn default_snapshot_root(
    app: &tauri::AppHandle,
    kind: SnapshotKind,
) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Other(format!("cannot locate app data directory: {e}")))?;
    Ok(dir.join("snapshots").join(kind.dir_name()))
}

/// How many snapshots [`prune_snapshots`] keeps per default root.
pub const SNAPSHOTS_TO_KEEP: usize = 5;

/// Remove all but the `keep` newest snapshot folders under `root`. Only
/// folders this module created (timestamp-named, containing `chat.db`) are
/// considered. Returns how many were removed; failures are logged and
/// skipped since pruning is housekeeping, not part of the safety contract.
pub fn prune_snapshots(root: &Path, keep: usize, just_taken: &Path) -> usize {
    let dirs = snapshot_dirs(root);
    let excess = dirs.len().saturating_sub(keep);
    let mut removed = 0;
    for (_, dir) in dirs.into_iter().take(excess) {
        // Never prune the snapshot protecting the operation in progress.
        if dir == just_taken {
            continue;
        }
        if remove_snapshot_dir(&dir) {
            removed += 1;
        }
    }
    removed
}

/// Remove snapshot folders under `root` written more than `max_age` before
/// `now`. Snapshots are full copies of the message history, so they
/// shouldn't outlive their usefulness as an undo. A snapshot dated in the
/// future (clock changes) counts as new. Returns how many were removed.
pub fn expire_snapshots(root: &Path, max_age: Duration, now: SystemTime) -> usize {
    snapshot_dirs(root)
        .into_iter()
        .filter(|((written, _, _), _)| now.duration_since(*written).is_ok_and(|age| age > max_age))
        .filter(|(_, dir)| remove_snapshot_dir(dir))
        .count()
}

/// Snapshot folders this module created under `root` (timestamp-named,
/// containing `chat.db`), oldest first.
fn snapshot_dirs(root: &Path) -> Vec<(SortKey, PathBuf)> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(SortKey, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let suffix = snapshot_suffix(&name)?;
            // Order by when the snapshot was actually written, not by its
            // name: names are local time, so after a DST fall-back or a
            // westward time-zone change a newer snapshot can sort as older.
            let written = fs::metadata(p.join("chat.db")).ok()?.modified().ok()?;
            Some(((written, name[..19].to_string(), suffix), p))
        })
        .collect();
    dirs.sort();
    dirs
}

fn remove_snapshot_dir(dir: &Path) -> bool {
    match fs::remove_dir_all(dir) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("snapshot: failed to remove {}: {e}", dir.display());
            false
        }
    }
}

/// (write time, timestamp part of the name, numeric `-N` suffix).
type SortKey = (SystemTime, String, u32);

/// How long snapshots are kept before [`expire_snapshots`] removes them.
/// Persisted in the app's data folder; `None` means "until replaced" (only
/// the newest [`SNAPSHOTS_TO_KEEP`] of each kind are kept either way).
pub struct SnapshotRetention {
    max_age_days: Mutex<Option<u32>>,
    path: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetentionFile {
    max_age_days: Option<u32>,
}

impl SnapshotRetention {
    pub const DEFAULT_DAYS: u32 = 30;
    /// Longest period the setting accepts.
    pub const MAX_DAYS: u32 = 3650;

    /// Load the setting saved at `path`, or the default if there is none.
    pub fn persistent(path: PathBuf) -> Self {
        let saved = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<RetentionFile>(&b).ok())
            .map_or(Some(Self::DEFAULT_DAYS), |f| f.max_age_days);
        Self {
            max_age_days: Mutex::new(saved),
            path: Some(path),
        }
    }

    pub fn max_age_days(&self) -> Option<u32> {
        *self.max_age_days.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn max_age(&self) -> Option<Duration> {
        self.max_age_days()
            .map(|d| Duration::from_secs(u64::from(d) * 24 * 60 * 60))
    }

    pub fn set(&self, days: Option<u32>) -> Result<(), AppError> {
        if days.is_some_and(|d| d == 0 || d > Self::MAX_DAYS) {
            return Err(AppError::Other(format!(
                "Keep snapshots for between 1 and {} days.",
                Self::MAX_DAYS
            )));
        }
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            fs::write(
                &tmp,
                serde_json::to_vec(&RetentionFile { max_age_days: days })
                    .map_err(|e| AppError::Other(e.to_string()))?,
            )?;
            fs::rename(&tmp, path)?;
        }
        *self.max_age_days.lock().unwrap_or_else(|p| p.into_inner()) = days;
        Ok(())
    }

    /// Remove the snapshots under `roots` that are older than the setting.
    pub fn expire(&self, roots: &[(SnapshotKind, PathBuf)]) -> usize {
        let Some(max_age) = self.max_age() else {
            return 0;
        };
        let now = SystemTime::now();
        roots
            .iter()
            .map(|(_, root)| expire_snapshots(root, max_age, now))
            .sum()
    }
}

impl Default for SnapshotRetention {
    /// The default period, not saved anywhere (tests, or no data folder).
    fn default() -> Self {
        Self {
            max_age_days: Mutex::new(Some(Self::DEFAULT_DAYS)),
            path: None,
        }
    }
}

/// A snapshot folder found on disk.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub path: String,
    pub kind: SnapshotKind,
    /// When its chat.db was written, in Unix seconds.
    pub taken_at_unix: u64,
    pub bytes: u64,
}

/// Every snapshot this app created under the given per-kind roots, newest
/// first.
pub fn list_snapshots(roots: &[(SnapshotKind, PathBuf)]) -> Vec<SnapshotInfo> {
    let mut out = Vec::new();
    for (kind, root) in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let named = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(snapshot_suffix)
                .is_some();
            let Ok(meta) = fs::metadata(path.join("chat.db")) else {
                continue;
            };
            if !named || !entry.file_type().is_ok_and(|t| t.is_dir()) || !meta.is_file() {
                continue;
            }
            let taken_at_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            out.push(SnapshotInfo {
                path: path.display().to_string(),
                kind: *kind,
                taken_at_unix,
                bytes: meta.len(),
            });
        }
    }
    out.sort_by(|a, b| {
        b.taken_at_unix
            .cmp(&a.taken_at_unix)
            .then(b.path.cmp(&a.path))
    });
    out
}

/// For a snapshot folder name (`YYYY-MM-DDTHH-MM-SS` plus an optional
/// `-N`), the numeric suffix (0 if none); `None` for other names.
fn snapshot_suffix(name: &str) -> Option<u32> {
    if !is_snapshot_name(name) {
        return None;
    }
    match name.get(19..) {
        Some("") | None => Some(0),
        Some(rest) => rest.strip_prefix('-')?.parse().ok(),
    }
}

fn is_snapshot_name(name: &str) -> bool {
    let (base, suffix) = name.split_at(name.len().min(19));
    chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%dT%H-%M-%S").is_ok()
        && (suffix.is_empty()
            || suffix
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a minimal SQLite db with a single row. Used only in tests —
    /// never touches the real chat.db.
    fn make_fixture_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT);
             INSERT INTO message (text) VALUES ('hello'), ('world');",
        )
        .unwrap();
    }

    #[test]
    fn snapshot_round_trips_row_counts() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("chat.db");
        make_fixture_db(&src);

        let snap_root = tmp.path().join("snaps");
        let snap = snapshot_chat_db(&src, &snap_root).unwrap();

        let snap_db = snap.dir.join("chat.db");
        assert!(snap_db.exists(), "snapshot db file should exist");

        let conn = Connection::open(&snap_db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn snapshot_is_isolated_from_source_mutations() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("chat.db");
        make_fixture_db(&src);

        let snap = snapshot_chat_db(&src, &tmp.path().join("snaps")).unwrap();

        // Mutate the source after snapshot — snapshot must not change.
        let w = Connection::open(&src).unwrap();
        w.execute("DELETE FROM message", []).unwrap();
        drop(w);

        let conn = Connection::open(snap.dir.join("chat.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "snapshot should preserve the pre-delete state");
    }

    #[test]
    fn back_to_back_snapshots_do_not_overwrite_each_other() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("chat.db");
        make_fixture_db(&src);
        let root = tmp.path().join("snaps");

        let first = snapshot_chat_db(&src, &root).unwrap();
        let w = Connection::open(&src).unwrap();
        w.execute("DELETE FROM message", []).unwrap();
        drop(w);
        let second = snapshot_chat_db(&src, &root).unwrap();

        assert_ne!(first.dir, second.dir);
        let conn = Connection::open(first.dir.join("chat.db")).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "first snapshot must survive a second one");
    }

    #[test]
    fn prune_keeps_newest_and_ignores_foreign_folders() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let names = [
            "2024-01-01T10-00-00",
            "2024-01-01T10-00-00-1",
            "2024-01-02T09-00-00",
            "2025-06-01T00-00-00",
        ];
        for n in names {
            fs::create_dir_all(root.join(n)).unwrap();
            fs::write(root.join(n).join("chat.db"), b"x").unwrap();
        }
        // Not ours: wrong name, or no chat.db.
        fs::create_dir_all(root.join("my-notes")).unwrap();
        fs::write(root.join("my-notes/chat.db"), b"x").unwrap();
        fs::create_dir_all(root.join("2020-01-01T00-00-00")).unwrap();

        assert_eq!(
            prune_snapshots(root, 2, &root.join("2025-06-01T00-00-00")),
            2
        );
        assert!(!root.join("2024-01-01T10-00-00").exists());
        assert!(!root.join("2024-01-01T10-00-00-1").exists());
        assert!(root.join("2024-01-02T09-00-00").exists());
        assert!(root.join("2025-06-01T00-00-00").exists());
        assert!(root.join("my-notes").exists());
        assert!(root.join("2020-01-01T00-00-00").exists());
    }

    #[test]
    fn prune_orders_by_write_time_and_spares_the_new_snapshot() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(10_000);
        // Taken in this order, but the clock went back an hour before the
        // last one (DST fall-back), so its name sorts first.
        let taken = [
            "2024-11-03T01-10-00",
            "2024-11-03T01-20-00",
            "2024-11-03T01-30-00",
            "2024-11-03T01-05-00",
        ];
        for (i, n) in taken.iter().enumerate() {
            let db = root.join(n).join("chat.db");
            fs::create_dir_all(db.parent().unwrap()).unwrap();
            fs::write(&db, b"x").unwrap();
            let f = fs::File::options().write(true).open(&db).unwrap();
            f.set_modified(base + std::time::Duration::from_secs(i as u64 * 60))
                .unwrap();
        }
        let newest = root.join("2024-11-03T01-05-00");

        assert_eq!(prune_snapshots(root, 2, &newest), 2);
        assert!(newest.exists(), "the snapshot just taken survives");
        assert!(root.join("2024-11-03T01-30-00").exists());
        assert!(!root.join("2024-11-03T01-10-00").exists());
        assert!(!root.join("2024-11-03T01-20-00").exists());

        // Even with keep = 0 the just-taken snapshot is never removed.
        assert_eq!(prune_snapshots(root, 0, &newest), 1);
        assert!(newest.exists());
    }

    /// A snapshot folder `root/name` whose chat.db was written `age` ago.
    fn snapshot_aged(root: &Path, name: &str, age: Duration) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("chat.db");
        fs::write(&db, b"x").unwrap();
        fs::File::options()
            .write(true)
            .open(&db)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
        dir
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn expiry_removes_only_old_snapshots_of_ours() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let old = snapshot_aged(root, "2024-01-01T00-00-00", 40 * DAY);
        let old_too = snapshot_aged(root, "2024-01-01T00-00-00-1", 31 * DAY);
        let recent = snapshot_aged(root, "2024-02-01T00-00-00", 29 * DAY);
        // Not ours, however old.
        let foreign = snapshot_aged(root, "my-notes", 400 * DAY);

        assert_eq!(expire_snapshots(root, 30 * DAY, SystemTime::now()), 2);
        assert!(!old.exists());
        assert!(!old_too.exists());
        assert!(recent.exists());
        assert!(foreign.exists());
    }

    #[test]
    fn a_snapshot_dated_in_the_future_is_kept() {
        let tmp = TempDir::new().unwrap();
        let dir = snapshot_aged(tmp.path(), "2024-01-01T00-00-00", Duration::ZERO);
        let long_ago = SystemTime::now() - 365 * DAY;
        assert_eq!(expire_snapshots(tmp.path(), DAY, long_ago), 0);
        assert!(dir.exists());
    }

    #[test]
    fn retention_setting_is_saved_and_applied() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("settings/snapshot_settings.json");
        let root = tmp.path().join("snaps");
        let roots = [(SnapshotKind::Delete, root.clone())];
        let week_old = snapshot_aged(&root, "2024-01-01T00-00-00", 8 * DAY);

        // Default: 30 days.
        let r = SnapshotRetention::persistent(file.clone());
        assert_eq!(r.max_age_days(), Some(SnapshotRetention::DEFAULT_DAYS));
        assert_eq!(r.expire(&roots), 0);

        // "Until replaced" survives a relaunch and expires nothing.
        r.set(None).unwrap();
        let r = SnapshotRetention::persistent(file.clone());
        assert_eq!(r.max_age_days(), None);
        assert_eq!(r.expire(&roots), 0);
        assert!(week_old.exists());

        r.set(Some(7)).unwrap();
        assert_eq!(SnapshotRetention::persistent(file).max_age_days(), Some(7));
        assert_eq!(r.expire(&roots), 1);
        assert!(!week_old.exists());

        assert!(r.set(Some(0)).is_err());
        assert!(r.set(Some(SnapshotRetention::MAX_DAYS + 1)).is_err());
        assert_eq!(r.max_age_days(), Some(7), "a refused value changes nothing");
    }

    #[test]
    fn numeric_suffixes_sort_numerically() {
        assert_eq!(snapshot_suffix("2024-01-01T00-00-00"), Some(0));
        assert_eq!(snapshot_suffix("2024-01-01T00-00-00-10"), Some(10));
        assert!(
            snapshot_suffix("2024-01-01T00-00-00-2") < snapshot_suffix("2024-01-01T00-00-00-10")
        );
        assert_eq!(snapshot_suffix("notes"), None);
    }

    #[test]
    fn snapshot_missing_source_errors() {
        let tmp = TempDir::new().unwrap();
        let err = snapshot_chat_db(&tmp.path().join("nope.db"), tmp.path()).unwrap_err();
        match err {
            AppError::Other(m) => assert!(m.contains("not found")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
