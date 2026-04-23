use std::fs;

use imessage_database::tables::{
    table::{get_connection, get_writable_connection},
    write::{
        attachment_filter::AttachmentFilter,
        delete::{execute_delete, preview_delete as lib_preview_delete},
    },
};
use serde::{Deserialize, Serialize};

use crate::core::db_path::default_chat_db_path;
use crate::core::filter::FilterSpec;
use crate::core::messages_app::is_messages_running;
use crate::core::snapshot::{checkpoint_wal, default_snapshot_root, snapshot_chat_db};
use crate::error::AppError;

pub const DELETE_CONFIRMATION_PHRASE: &str = "DELETE";

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

    let plan =
        lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).map_err(|e| {
            AppError::Database(format!("failed to compute delete plan: {e}"))
        })?;

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
    /// Caller must have already acknowledged a prior `run_backup` that
    /// covers the same filter. The frontend enforces this by only enabling
    /// the delete button after a successful backup; the backend treats it
    /// as a soft gate: `false` is allowed (backup-skipped delete) but
    /// should be accompanied by an extra confirmation on the frontend.
    #[serde(default)]
    pub backup_verified: bool,
    /// Optional override for where snapshots are stored. Defaults to
    /// `~/Documents/iMessage Backups/snapshots/<timestamp>/`.
    pub snapshot_root: Option<String>,
    /// Controls which parts of the matched records are removed.
    #[serde(default)]
    pub delete_scope: DeleteScope,
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

    // Gate 2: Messages.app must be quit. We err on the side of refusing
    // when we can't tell (pgrep unavailable → treat as "could be running").
    if is_messages_running().unwrap_or(true) {
        return Err(AppError::Other(
            "Quit Messages.app before running a delete — it holds a write lock on chat.db."
                .into(),
        ));
    }

    let db_path = default_chat_db_path()?;

    // Gate 3: mandatory WAL checkpoint + snapshot.
    let snapshot_root = match args.snapshot_root.as_deref() {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => default_snapshot_root()?,
    };

    checkpoint_wal(&db_path)?;
    let snapshot = snapshot_chat_db(&db_path, &snapshot_root)?;

    // Build the delete plan against a read-only connection — even though
    // we'll execute it against a writable one, there's no reason to hold
    // two writers, and the read-only path is more conservative.
    let read_conn = get_connection(&db_path)?;
    let ctx = args.filter.to_query_context()?;
    let mut plan =
        lib_preview_delete(&read_conn, &ctx, &AttachmentFilter::any(), &db_path).map_err(
            |e| AppError::Database(format!("failed to compute delete plan: {e}")),
        )?;
    drop(read_conn);

    // Apply the requested scope: clear the parts the caller wants to keep.
    match args.delete_scope {
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

    // Unlink attachment files from disk. Failures are non-fatal.
    let mut removed = 0u64;
    let mut failed = 0u64;
    for p in &plan.attachment_files_on_disk {
        if p.exists() {
            match fs::remove_file(p) {
                Ok(()) => removed += 1,
                Err(_) => failed += 1,
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
