use std::fs;

use imessage_database::tables::{
    table::{get_connection, get_writable_connection},
    write::{
        attachment_filter::AttachmentFilter,
        delete::{execute_delete, preview_delete as lib_preview_delete, DeletePlan},
    },
};
use serde::{Deserialize, Serialize};

use crate::core::db_path::default_chat_db_path;
use crate::core::filter::FilterSpec;
use crate::core::icloud::{detect_icloud_messages, ICloudState};
use crate::core::messages_app::is_messages_running;
use crate::core::snapshot::{checkpoint_wal, default_snapshot_root, snapshot_chat_db};
use crate::error::AppError;

pub const DELETE_CONFIRMATION_PHRASE: &str = "DELETE";

/// Apply a `DeleteScope` to a freshly-computed plan, zeroing out the parts
/// the caller wants to keep. Pulled out of `run_delete` so the slicing
/// logic is covered by unit tests without needing a real chat.db.
pub(crate) fn apply_scope(plan: &mut DeletePlan, scope: &DeleteScope) {
    match scope {
        DeleteScope::MessagesOnly => {
            // Keep attachment records and files on disk; only remove messages.
            plan.attachment_rowids.clear();
            plan.attachment_files_on_disk.clear();
            plan.attachment_bytes = 0;
        }
        DeleteScope::AttachmentsOnly => {
            // Keep messages; only remove attachment records and files.
            plan.message_rowids.clear();
        }
        DeleteScope::Both => {}
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePreview {
    pub message_count: u64,
    pub attachment_count: u64,
    pub attachment_bytes: u64,
    pub on_disk_file_count: u64,
}

/// Compute what a delete with the given filter would remove. Read-only.
#[tauri::command]
pub async fn preview_delete(filter: FilterSpec) -> Result<DeletePreview, AppError> {
    let db_path = default_chat_db_path()?;
    let conn = get_connection(&db_path)?;
    let ctx = filter.to_query_context()?;

    let plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path)
        .map_err(|e| AppError::Database(format!("failed to compute delete plan: {e}")))?;

    Ok(DeletePreview {
        message_count: plan.message_rowids.len() as u64,
        attachment_count: plan.attachment_rowids.len() as u64,
        attachment_bytes: plan.attachment_bytes,
        on_disk_file_count: plan.attachment_files_on_disk.len() as u64,
    })
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeleteScope {
    /// Delete both messages and their attachment records/files (default).
    #[default]
    Both,
    /// Delete message rows and join tables only; leave attachment records
    /// and files on disk untouched.
    MessagesOnly,
    /// Delete attachment records and files only; leave message rows intact
    /// so conversation history is preserved.
    AttachmentsOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDeleteArgs {
    pub filter: FilterSpec,
    pub confirmation_phrase: String,
    /// Set to `true` when the caller has already run a matching `run_backup`
    /// in this session. If `false`, `acknowledge_skip_backup` must be
    /// explicitly `true` — the backend no longer silently permits
    /// backup-less deletes.
    #[serde(default)]
    pub backup_verified: bool,
    /// Required when `backup_verified` is `false`. A direct-IPC caller must
    /// actively opt out of the backup recommendation rather than default
    /// into it.
    #[serde(default)]
    pub acknowledge_skip_backup: bool,
    /// Optional override for where snapshots are stored. Defaults to
    /// `~/Documents/iMessage Backups/snapshots/<timestamp>/`.
    pub snapshot_root: Option<String>,
    /// Controls which parts of the matched records are removed.
    #[serde(default)]
    pub delete_scope: DeleteScope,
    /// Must be `true` when Messages in iCloud is detected as enabled. A
    /// local delete on a synced account can be re-synced from cloud state,
    /// so the caller has to prove they understand the consequences.
    #[serde(default)]
    pub acknowledge_icloud_sync: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResult {
    pub messages_deleted: u64,
    pub attachments_deleted: u64,
    pub attachment_joins_deleted: u64,
    pub chat_message_joins_deleted: u64,
    pub orphan_chats_deleted: u64,
    pub orphan_handles_deleted: u64,
    pub snapshot_path: String,
    pub on_disk_files_removed: u64,
    pub on_disk_files_failed: u64,
    pub backup_verified: bool,
}

/// Execute a delete. Enforces the full safety contract: typed
/// confirmation, Messages.app-not-running, mandatory snapshot with a WAL
/// checkpoint, then a transactional delete against a writable connection.
/// Attachment files on disk are unlinked **after** the SQL transaction
/// commits; individual unlink failures are counted but do not roll back.
#[tauri::command]
pub async fn run_delete(args: RunDeleteArgs) -> Result<DeleteResult, AppError> {
    // Gate 1: typed confirmation.
    if args.confirmation_phrase != DELETE_CONFIRMATION_PHRASE {
        return Err(AppError::Other(format!(
            "confirmation phrase must be exactly '{DELETE_CONFIRMATION_PHRASE}'"
        )));
    }

    // Gate 1a: backup gate. If the caller didn't back up first, they must
    // explicitly acknowledge the skip. This turns the previously-advisory
    // flag into a real check that survives a frontend regression or a
    // direct-IPC caller.
    if !args.backup_verified && !args.acknowledge_skip_backup {
        return Err(AppError::Other(
            "No backup has been run for this filter. Run a backup first, or pass \
             acknowledgeSkipBackup=true to proceed without one."
                .into(),
        ));
    }

    // Gate 2: Messages.app must be quit. We err on the side of refusing
    // when we can't tell (pgrep unavailable → treat as "could be running").
    if is_messages_running().unwrap_or(true) {
        return Err(AppError::Other(
            "Quit Messages.app before running a delete — it holds a write lock on chat.db.".into(),
        ));
    }

    // Gate 2a: Messages-in-iCloud refuses-to-proceed without explicit ack.
    // When detection is Unknown we don't block (false positives would be
    // worse than a soft UI warning), but Enabled is hard-gated.
    if detect_icloud_messages().unwrap_or(ICloudState::Unknown) == ICloudState::Enabled
        && !args.acknowledge_icloud_sync
    {
        return Err(AppError::Other(
            "Messages in iCloud is enabled. A local delete may be re-synced from cloud. \
             Acknowledge the iCloud warning in the UI (or pass acknowledgeIcloudSync=true) \
             before retrying."
                .into(),
        ));
    }

    let db_path = default_chat_db_path()?;

    // Gate 3: mandatory WAL checkpoint + snapshot.
    let snapshot_root = match args.snapshot_root.as_deref() {
        Some(p) if !p.is_empty() => crate::core::paths::validate_user_path(p, "snapshot root")?,
        _ => default_snapshot_root()?,
    };

    checkpoint_wal(&db_path)?;
    let snapshot = snapshot_chat_db(&db_path, &snapshot_root)?;

    // Build the delete plan against a read-only connection — even though
    // we'll execute it against a writable one, there's no reason to hold
    // two writers, and the read-only path is more conservative.
    let read_conn = get_connection(&db_path)?;
    let ctx = args.filter.to_query_context()?;
    let mut plan = lib_preview_delete(&read_conn, &ctx, &AttachmentFilter::any(), &db_path)
        .map_err(|e| AppError::Database(format!("failed to compute delete plan: {e}")))?;
    drop(read_conn);

    apply_scope(&mut plan, &args.delete_scope);

    // Now execute against a writable connection.
    let mut write_conn = get_writable_connection(&db_path)?;
    let report = match execute_delete(&mut write_conn, &plan) {
        Ok(r) => r,
        Err(e) => {
            return Err(AppError::Database(format!(
                "delete transaction failed (rolled back; no changes made): {e}"
            )));
        }
    };
    // Release the writable handle immediately so Messages.app can reopen.
    drop(write_conn);

    // Unlink attachment files from disk. Failures are non-fatal — the DB is
    // the source of truth, so a stale file on disk is a minor janitor issue,
    // not a data-integrity event. Still, we emit the path + reason to stderr
    // so the user has something to grep after the fact.
    let mut removed = 0u64;
    let mut failed = 0u64;
    for p in &plan.attachment_files_on_disk {
        if p.exists() {
            match fs::remove_file(p) {
                Ok(()) => removed += 1,
                Err(e) => {
                    eprintln!(
                        "delete: failed to remove attachment file {}: {e}",
                        p.display()
                    );
                    failed += 1;
                }
            }
        }
    }

    Ok(DeleteResult {
        messages_deleted: report.messages_deleted as u64,
        attachments_deleted: report.attachments_deleted as u64,
        attachment_joins_deleted: report.attachment_joins_deleted as u64,
        chat_message_joins_deleted: report.chat_message_joins_deleted as u64,
        orphan_chats_deleted: report.orphan_chats_deleted as u64,
        orphan_handles_deleted: report.orphan_handles_deleted as u64,
        snapshot_path: snapshot.dir.display().to_string(),
        on_disk_files_removed: removed,
        on_disk_files_failed: failed,
        backup_verified: args.backup_verified,
    })
}

#[cfg(test)]
mod tests {
    //! Integration-style tests for the destructive path. Every test
    //! operates on a `TempDir` fixture — **never** on the real
    //! `~/Library/Messages/chat.db`.
    use super::*;
    use imessage_database::util::query_context::QueryContext;
    use rusqlite::{params, Connection};
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Build a minimal chat.db-shaped schema. Mirrors the fixture the
    /// library's own delete tests use, pared down to the columns our
    /// integration touches.
    fn fixture_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE message (
                ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                guid TEXT NOT NULL,
                text TEXT,
                service TEXT,
                handle_id INTEGER,
                date INTEGER NOT NULL,
                is_from_me INTEGER DEFAULT 0
            );
            CREATE TABLE chat (
                ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                chat_identifier TEXT NOT NULL,
                service_name TEXT,
                display_name TEXT
            );
            CREATE TABLE handle (
                ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL,
                person_centric_id TEXT
            );
            CREATE TABLE attachment (
                ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                filename TEXT,
                mime_type TEXT,
                total_bytes INTEGER DEFAULT 0,
                transfer_name TEXT,
                uti TEXT,
                is_sticker INTEGER DEFAULT 0,
                hide_attachment INTEGER DEFAULT 0
            );
            CREATE TABLE chat_message_join (
                chat_id INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                PRIMARY KEY (chat_id, message_id)
            );
            CREATE TABLE message_attachment_join (
                message_id INTEGER NOT NULL,
                attachment_id INTEGER NOT NULL,
                PRIMARY KEY (message_id, attachment_id)
            );
            CREATE TABLE chat_handle_join (
                chat_id INTEGER NOT NULL,
                handle_id INTEGER NOT NULL,
                PRIMARY KEY (chat_id, handle_id)
            );",
        )
        .unwrap();
    }

    /// Seed 2 messages in 1 chat with 1 attachment row. Returns the
    /// attachment file's on-disk path for cleanup-path assertions.
    fn seed(conn: &Connection, tmp: &TempDir) -> PathBuf {
        conn.execute_batch(
            "INSERT INTO chat (ROWID, chat_identifier) VALUES (1, '+15551234567');
             INSERT INTO handle (ROWID, id) VALUES (1, '+15551234567');
             INSERT INTO chat_handle_join VALUES (1, 1);
             INSERT INTO message (ROWID, guid, text, service, handle_id, date, is_from_me)
               VALUES (1, 'g1', 'hello', 'iMessage', 1, 0, 0);
             INSERT INTO message (ROWID, guid, text, service, handle_id, date, is_from_me)
               VALUES (2, 'g2', 'world', 'iMessage', 1, 0, 1);
             INSERT INTO chat_message_join VALUES (1, 1), (1, 2);",
        )
        .unwrap();

        // Create a physical file the attachment row points at.
        let att_path = tmp.path().join("att1.jpg");
        std::fs::write(&att_path, b"fake-jpeg-bytes").unwrap();

        conn.execute(
            "INSERT INTO attachment (ROWID, filename, mime_type, total_bytes, transfer_name, uti)
             VALUES (1, ?1, 'image/jpeg', 15, 'att1.jpg', 'public.jpeg')",
            params![att_path.to_str().unwrap()],
        )
        .unwrap();
        conn.execute("INSERT INTO message_attachment_join VALUES (2, 1)", [])
            .unwrap();

        att_path
    }

    fn fresh_fixture() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("chat.db");
        let conn = Connection::open(&db_path).unwrap();
        fixture_schema(&conn);
        let att = seed(&conn, &tmp);
        drop(conn);
        (tmp, db_path, att)
    }

    #[test]
    fn apply_scope_messages_only_keeps_attachments() {
        let mut plan = DeletePlan {
            message_rowids: vec![1, 2],
            attachment_rowids: vec![10],
            attachment_files_on_disk: vec![PathBuf::from("/tmp/x.jpg")],
            attachment_bytes: 1024,
            cleanup_orphans: true,
        };
        apply_scope(&mut plan, &DeleteScope::MessagesOnly);
        assert_eq!(plan.message_rowids, vec![1, 2]);
        assert!(plan.attachment_rowids.is_empty());
        assert!(plan.attachment_files_on_disk.is_empty());
        assert_eq!(plan.attachment_bytes, 0);
    }

    #[test]
    fn apply_scope_attachments_only_keeps_messages() {
        let mut plan = DeletePlan {
            message_rowids: vec![1, 2],
            attachment_rowids: vec![10],
            attachment_files_on_disk: vec![PathBuf::from("/tmp/x.jpg")],
            attachment_bytes: 1024,
            cleanup_orphans: true,
        };
        apply_scope(&mut plan, &DeleteScope::AttachmentsOnly);
        assert!(plan.message_rowids.is_empty());
        assert_eq!(plan.attachment_rowids, vec![10]);
        assert_eq!(plan.attachment_files_on_disk.len(), 1);
        assert_eq!(plan.attachment_bytes, 1024);
    }

    #[test]
    fn apply_scope_both_is_pass_through() {
        let original = DeletePlan {
            message_rowids: vec![1, 2],
            attachment_rowids: vec![10, 11],
            attachment_files_on_disk: vec![PathBuf::from("/tmp/x.jpg")],
            attachment_bytes: 1024,
            cleanup_orphans: true,
        };
        let mut plan = original.clone();
        apply_scope(&mut plan, &DeleteScope::Both);
        assert_eq!(plan.message_rowids, original.message_rowids);
        assert_eq!(plan.attachment_rowids, original.attachment_rowids);
        assert_eq!(plan.attachment_bytes, original.attachment_bytes);
    }

    #[test]
    fn preview_on_fixture_counts_all_messages() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = Connection::open(&db_path).unwrap();
        let ctx = QueryContext::default();
        let plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).unwrap();
        assert_eq!(plan.message_rowids.len(), 2);
        assert_eq!(plan.attachment_rowids.len(), 1);
        assert_eq!(plan.attachment_bytes, 15);
    }

    #[test]
    fn execute_delete_removes_all_rows_and_tracks_files_on_disk() {
        let (tmp, db_path, att) = fresh_fixture();
        assert!(att.exists());

        let mut conn = Connection::open(&db_path).unwrap();
        let ctx = QueryContext::default();
        let plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).unwrap();
        let report = execute_delete(&mut conn, &plan).unwrap();
        assert_eq!(report.messages_deleted, 2);
        assert_eq!(report.attachments_deleted, 1);

        // SQL transaction committed — rows are gone.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        // The file on disk is the caller's (run_delete's) responsibility to
        // unlink after commit. Simulate that step and verify the file is
        // actually removed so we don't regress the unlink logic silently.
        for p in &plan.attachment_files_on_disk {
            if p.exists() {
                std::fs::remove_file(p).unwrap();
            }
        }
        assert!(!att.exists());

        drop(tmp);
    }

    #[test]
    fn scoped_messages_only_delete_leaves_attachment_rows_and_file_intact() {
        let (tmp, db_path, att) = fresh_fixture();
        let mut conn = Connection::open(&db_path).unwrap();
        let ctx = QueryContext::default();
        let mut plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).unwrap();
        apply_scope(&mut plan, &DeleteScope::MessagesOnly);
        execute_delete(&mut conn, &plan).unwrap();

        // Messages gone…
        let n_msg: i64 = conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_msg, 0);
        // …but attachment row and file survive.
        let n_att: i64 = conn
            .query_row("SELECT COUNT(*) FROM attachment", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_att, 1);
        assert!(att.exists());

        drop(tmp);
    }

    #[test]
    fn snapshot_of_fixture_is_independent_of_subsequent_delete() {
        let (tmp, db_path, _att) = fresh_fixture();
        let snap_root = tmp.path().join("snaps");

        let snap = crate::core::snapshot::snapshot_chat_db(&db_path, &snap_root).unwrap();

        // Execute the delete on the source db.
        let mut conn = Connection::open(&db_path).unwrap();
        let ctx = QueryContext::default();
        let plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).unwrap();
        execute_delete(&mut conn, &plan).unwrap();
        drop(conn);

        // The snapshot should still show the pre-delete state.
        let snap_conn = Connection::open(snap.dir.join("chat.db")).unwrap();
        let n: i64 = snap_conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "snapshot must preserve rows deleted from source");
    }
}
