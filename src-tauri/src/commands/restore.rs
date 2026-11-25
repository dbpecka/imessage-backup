//! Restore chat.db from a snapshot taken before a delete or cleanup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::delete::SafetyProbe;
use crate::core::db_path::{blocking, default_chat_db_path, open_write};
use crate::core::icloud::ICloudState;
use crate::core::op_lock::OperationLock;
use crate::core::snapshot::{
    checkpoint_wal, default_snapshot_root, list_snapshots as find_snapshots, prune_snapshots,
    snapshot_chat_db, SnapshotInfo, SnapshotKind, SnapshotRetention, SNAPSHOTS_TO_KEEP,
};
use crate::core::space::{ensure_space, snapshot_size};
use crate::error::AppError;

pub(crate) fn snapshot_roots(app: &AppHandle) -> Result<Vec<(SnapshotKind, PathBuf)>, AppError> {
    SnapshotKind::ALL
        .iter()
        .map(|k| Ok((*k, default_snapshot_root(app, *k)?)))
        .collect()
}

/// Snapshots available to restore, newest first.
#[tauri::command]
pub async fn list_snapshots(app: AppHandle) -> Result<Vec<SnapshotInfo>, AppError> {
    let roots = snapshot_roots(&app)?;
    blocking(move || Ok(find_snapshots(&roots))).await
}

/// Remove snapshots older than the retention setting. Housekeeping: a
/// failure is logged, never reported. Call with the operation lock held, so
/// a restore can't be reading a snapshot this removes.
pub(crate) async fn expire_old_snapshots(app: &AppHandle) {
    let Ok(roots) = snapshot_roots(app) else {
        return;
    };
    let retention = Arc::clone(&app.state::<Arc<SnapshotRetention>>());
    let removed = tokio::task::spawn_blocking(move || retention.expire(&roots))
        .await
        .unwrap_or(0);
    if removed > 0 {
        // A snapshot list on screen may now name folders that are gone.
        app.emit("snapshots-changed", ()).ok();
    }
}

/// How many days snapshots are kept; `None` means until replaced.
#[tauri::command]
pub fn get_snapshot_retention(retention: State<'_, Arc<SnapshotRetention>>) -> Option<u32> {
    retention.max_age_days()
}

/// Change how long snapshots are kept, and remove any that are now too old.
#[tauri::command]
pub async fn set_snapshot_retention(
    app: AppHandle,
    retention: State<'_, Arc<SnapshotRetention>>,
    lock: State<'_, OperationLock>,
    days: Option<u32>,
) -> Result<(), AppError> {
    retention.set(days)?;
    match lock.try_acquire("a snapshot cleanup") {
        Ok(_guard) => expire_old_snapshots(&app).await,
        // Something is running (a backup, say): apply the new setting as
        // soon as it finishes.
        Err(_) => {
            let lock = lock.inner().clone();
            tauri::async_runtime::spawn(async move {
                let _guard = lock.acquire().await;
                expire_old_snapshots(&app).await;
            });
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSnapshotArgs {
    /// A `path` from [`list_snapshots`].
    pub snapshot_path: String,
}

/// Delete one snapshot now.
#[tauri::command]
pub async fn delete_snapshot(
    app: AppHandle,
    lock: State<'_, OperationLock>,
    args: DeleteSnapshotArgs,
) -> Result<(), AppError> {
    let _guard = lock.try_acquire("deleting a snapshot")?;
    let roots = snapshot_roots(&app)?;
    blocking(move || delete_snapshot_now(&roots, &args.snapshot_path)).await
}

pub(crate) fn delete_snapshot_now(
    roots: &[(SnapshotKind, PathBuf)],
    snapshot_path: &str,
) -> Result<(), AppError> {
    let dir = resolve_snapshot(snapshot_path, roots)?;
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreArgs {
    /// A `path` from [`list_snapshots`].
    pub snapshot_path: String,
    /// Required unless Messages in iCloud is positively detected as off:
    /// how iCloud reconciles a database rolled back underneath it is
    /// unpredictable.
    #[serde(default)]
    pub acknowledge_icloud_sync: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub restored_from: String,
    /// The database as it was just before the restore, so the restore
    /// itself can be undone.
    pub pre_restore_snapshot: String,
}

#[tauri::command]
pub async fn restore_snapshot(
    app: AppHandle,
    lock: State<'_, OperationLock>,
    args: RestoreArgs,
) -> Result<RestoreResult, AppError> {
    let _guard = lock.try_acquire("a restore")?;
    let db_path = default_chat_db_path()?;
    let roots = snapshot_roots(&app)?;
    let result =
        blocking(move || restore_now(&db_path, &roots, &args, &SafetyProbe::system())).await?;
    expire_old_snapshots(&app).await;
    Ok(result)
}

/// The snapshot folder, if it is one this app created: directly inside one
/// of `roots`, holding a `chat.db`. Anything else is refused — this command
/// overwrites the live database, so it must never take an arbitrary path.
fn resolve_snapshot(
    snapshot_path: &str,
    roots: &[(SnapshotKind, PathBuf)],
) -> Result<PathBuf, AppError> {
    let refuse = || {
        AppError::Other(format!(
            "{snapshot_path} isn't a Bubble Wrap snapshot. Pick one from the list."
        ))
    };
    let dir = std::fs::canonicalize(snapshot_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            && roots
                .iter()
                .any(|(_, root)| Path::new(snapshot_path).parent() == Some(root.as_path()))
        {
            AppError::Other(
                "That snapshot no longer exists (it may have expired or been deleted).".into(),
            )
        } else {
            refuse()
        }
    })?;
    let in_a_root = roots.iter().any(|(_, root)| {
        std::fs::canonicalize(root).is_ok_and(|root| dir.parent() == Some(root.as_path()))
    });
    let listed = find_snapshots(roots)
        .iter()
        .any(|s| std::fs::canonicalize(&s.path).is_ok_and(|p| p == dir));
    if !in_a_root || !listed {
        return Err(refuse());
    }
    Ok(dir)
}

/// Refuse a snapshot SQLite doesn't consider intact, or that isn't a
/// Messages database at all.
fn check_snapshot(db: &Path) -> Result<(), AppError> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| AppError::Other(format!("can't open snapshot: {e}")))?;
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| AppError::Other(format!("snapshot is damaged: {e}")))?;
    if result != "ok" {
        return Err(AppError::Other(format!("snapshot is damaged: {result}")));
    }
    let has_messages: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'message')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| AppError::Other(format!("can't read snapshot: {e}")))?;
    if !has_messages {
        return Err(AppError::Other(
            "that snapshot isn't a Messages database".into(),
        ));
    }
    Ok(())
}

/// Replace the live chat.db's contents with a snapshot's (see
/// [`restore_snapshot`]). Attachment files deleted since aren't in the
/// snapshot and stay deleted.
pub(crate) fn restore_now(
    db_path: &Path,
    roots: &[(SnapshotKind, PathBuf)],
    args: &RestoreArgs,
    probe: &SafetyProbe,
) -> Result<RestoreResult, AppError> {
    let snapshot_dir = resolve_snapshot(&args.snapshot_path, roots)?;
    let snapshot_db = snapshot_dir.join("chat.db");
    check_snapshot(&snapshot_db)?;

    if (probe.messages_running)() {
        return Err(AppError::Other(
            "Quit Messages.app before restoring a snapshot.".into(),
        ));
    }
    if (probe.icloud)() != ICloudState::Disabled && !args.acknowledge_icloud_sync {
        return Err(AppError::Other(
            "Messages in iCloud may be on. Restoring rolls this Mac's database back underneath \
             it, and how iCloud reconciles that is unpredictable. Acknowledge the iCloud warning \
             (or pass acknowledgeIcloudSync=true) before retrying."
                .into(),
        ));
    }

    // Keep the current state first, so the restore can be undone too.
    let pre_root = roots
        .iter()
        .find(|(k, _)| *k == SnapshotKind::PreRestore)
        .map(|(_, r)| r.clone())
        .ok_or_else(|| AppError::Other("no pre-restore snapshot folder".into()))?;
    ensure_space(
        &pre_root,
        snapshot_size(db_path),
        "a snapshot of the current database",
        probe.available_space,
    )?;
    checkpoint_wal(db_path)?;
    let pre = snapshot_chat_db(db_path, &pre_root)?;

    if (probe.messages_running)() {
        return Err(AppError::Other(
            "Messages.app was opened during the restore — quit it and try again. Nothing was \
             changed."
                .into(),
        ));
    }

    // SQLite's backup API copies the snapshot page by page into the live
    // database under its own locks, which keeps the write-ahead log
    // consistent (unlike swapping files underneath it).
    let src = Connection::open_with_flags(
        &snapshot_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut live = open_write(db_path)?;
    {
        let backup = Backup::new(&src, &mut live)
            .map_err(|e| AppError::Other(format!("couldn't start restore: {e}")))?;
        backup
            .run_to_completion(1024, std::time::Duration::from_millis(0), None)
            .map_err(|e| AppError::Other(format!("restore failed: {e}")))?;
    }
    drop(live);
    prune_snapshots(&pre_root, SNAPSHOTS_TO_KEEP, &pre.dir);

    Ok(RestoreResult {
        restored_from: snapshot_dir.display().to_string(),
        pre_restore_snapshot: pre.dir.display().to_string(),
    })
}
