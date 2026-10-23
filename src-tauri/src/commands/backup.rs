use imessage_database::{
    exporters::{
        compatibility::attachment_manager::AttachmentManagerMode,
        config::{AttachmentMode, ExportConfig},
        export_type::ExportType,
        json::run_json_export,
        options::Options,
        pdf::run_pdf_export,
        progress::ProgressReporter,
        runtime::Config,
    },
    tables::messages::message::Message,
};
use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::commands::delete::filter_content;
use crate::core::attachment_copy::{
    annotate_json_export, copy_attachments, verify_runtime_copies, CopySummary,
};
use crate::core::db_path::{blocking, check_chat_db_access, default_chat_db_path, open_read};
use crate::core::filter::FilterSpec;
use crate::core::op_lock::OperationLock;
use crate::core::progress::TauriProgress;
use crate::core::receipts::{BackupReceipts, ContentFingerprint};
use crate::core::snapshot::create_unique_dir;
use crate::error::AppError;

/// Write the tapbacks in `ids` to `tapbacks-outside-filter.json`.
fn write_extra_tapbacks(
    conn: &rusqlite::Connection,
    export_path: &std::path::Path,
    ids: &[i64],
) -> Result<(), AppError> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT ROWID, guid, associated_message_guid, associated_message_type, date, \
                    handle_id, is_from_me, text \
             FROM message WHERE ROWID IN ({placeholders})"
        ))?;
        let mapped = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok(serde_json::json!({
                "rowid": r.get::<_, i64>(0)?,
                "guid": r.get::<_, Option<String>>(1)?,
                "associatedMessageGuid": r.get::<_, Option<String>>(2)?,
                "associatedMessageType": r.get::<_, Option<i64>>(3)?,
                "date": r.get::<_, Option<i64>>(4)?,
                "handleId": r.get::<_, Option<i64>>(5)?,
                "isFromMe": r.get::<_, Option<i64>>(6)?,
                "text": r.get::<_, Option<String>>(7)?,
            }))
        })?;
        for row in mapped {
            rows.push(row?);
        }
    }
    let json = serde_json::to_vec_pretty(&rows).map_err(|e| AppError::Other(e.to_string()))?;
    std::fs::write(export_path.join("tapbacks-outside-filter.json"), json)?;
    Ok(())
}

fn count(r: Result<i64, imessage_database::error::table::TableError>) -> Result<u64, AppError> {
    r.map(|n| n.max(0) as u64)
        .map_err(|e| AppError::Other(e.to_string()))
}

pub const BACKUP_PROGRESS_EVENT: &str = "backup-progress";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBackupArgs {
    pub filter: FilterSpec,
    pub format: String,
    pub destination: String,
    #[serde(default)]
    pub copy_attachments: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    pub message_count: u64,
    pub attachment_count: u64,
    pub attachment_bytes_copied: u64,
    pub conversation_count: u64,
    pub manifest_path: String,
    /// The folder this run wrote into (a fresh subfolder of the chosen
    /// destination).
    pub export_path: String,
    pub format: String,
    /// Whether every attachment on this Mac was copied into the export. A
    /// backup that only references attachments, or where some copies
    /// failed, doesn't protect them from a delete.
    pub includes_attachments: bool,
    /// Per-attachment copy outcomes; `None` when attachments weren't copied.
    pub attachment_copy: Option<CopySummary>,
}

/// Run a backup with the supplied filter and format into a new, timestamped
/// subfolder of the supplied destination. Emits progress events on
/// `backup-progress`.
#[tauri::command]
pub async fn run_backup(
    app: AppHandle,
    receipts: State<'_, Arc<BackupReceipts>>,
    lock: State<'_, OperationLock>,
    args: RunBackupArgs,
) -> Result<BackupResult, AppError> {
    // A delete or cleanup running alongside could remove files mid-copy.
    let _guard = lock.try_acquire("a backup")?;
    let db_path = default_chat_db_path()?;
    let receipts = Arc::clone(&receipts);
    let progress = TauriProgress::new(app.clone(), BACKUP_PROGRESS_EVENT);
    blocking(move || backup_now(&db_path, &receipts, args, &progress)).await
}

/// The whole backup as a plain function (see [`run_backup`]): export,
/// attachment copy, and the receipt that lets a later delete rely on it.
pub(crate) fn backup_now(
    db_path: &std::path::Path,
    receipts: &BackupReceipts,
    args: RunBackupArgs,
    progress: &dyn ProgressReporter,
) -> Result<BackupResult, AppError> {
    let format = ExportType::from_cli(&args.format)
        .ok_or_else(|| AppError::Other(format!("unsupported format: {}", args.format)))?;
    check_chat_db_access(db_path)?;
    let db_path = db_path.to_path_buf();
    let destination = crate::core::paths::validate_user_path(&args.destination, "destination")?;
    let filter_key = args.filter.key();
    let copy = args.copy_attachments;
    let query = args.filter.to_query_context()?;
    let format_label = format.to_string();

    // Fingerprint what the filter matches *before* exporting. Anything
    // that arrives during or after the run changes the fingerprint, so
    // this backup can never vouch for messages it might not contain.
    let (content, planned_attachments, reaction_ids) = {
        let conn = open_read(&db_path)?;
        let fc = filter_content(&conn, &query, &db_path)?;
        (fc.content, fc.plan.attachment_rowids, fc.reaction_ids)
    };

    // Every run gets its own folder: the JSON exporter appends to
    // existing files (re-running into the same folder duplicated every
    // message), and stale files from an earlier run must never be
    // mistaken for this run's copies.
    std::fs::create_dir_all(&destination)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M.%S").to_string();
    let export_path = create_unique_dir(&destination, &format!("Bubble Wrap {stamp}"))?;

    // The library's own attachment copying is collision-prone (see
    // `attachment_copy`), so JSON/PDF always run in reference mode and
    // we copy separately. The HTML/TXT runtime names copies safely by
    // rowid and links to them, so it keeps doing its own copying and we
    // verify the result instead of copying everything twice.
    let config = |mode: AttachmentMode| ExportConfig {
        db_path: db_path.clone(),
        export_path: export_path.clone(),
        format,
        query: query.clone(),
        attachments: mode,
        attachment_root: None,
        custom_owner_name: None,
    };
    let export_label = export_path.display().to_string();

    let (message_count, attachment_count, conversation_count, manifest_path, copy_summary) =
        match format {
            ExportType::Json => {
                let summary = run_json_export(&config(AttachmentMode::Reference), progress)
                    .map_err(|e| AppError::Other(e.to_string()))?;
                let copy_summary = if copy {
                    let conn = open_read(&db_path)?;
                    let (cs, copied) =
                        copy_attachments(&conn, &db_path, &query, &export_path, progress)?;
                    annotate_json_export(&export_path, &summary.manifest_path, &copied, &cs)?;
                    Some(cs)
                } else {
                    None
                };
                (
                    summary.message_count,
                    summary.attachment_count,
                    summary.conversation_count,
                    summary.manifest_path.display().to_string(),
                    copy_summary,
                )
            }
            ExportType::Pdf => {
                // Copy first so the PDF run leaves a browsable attachment
                // archive next to the PDF.
                let copy_summary = if copy {
                    let conn = open_read(&db_path)?;
                    Some(copy_attachments(&conn, &db_path, &query, &export_path, progress)?.0)
                } else {
                    None
                };
                let summary = run_pdf_export(&config(AttachmentMode::Reference), progress)
                    .map_err(|e| AppError::Other(e.to_string()))?;
                (
                    summary.message_count,
                    summary.attachment_count,
                    summary.conversation_count,
                    String::new(),
                    copy_summary,
                )
            }
            ExportType::Html | ExportType::Txt => {
                // The library's HTML/TXT runtime doesn't return a
                // summary, so compute counts from the same filter first.
                let (messages, conversations, attachments) = {
                    let conn = open_read(&db_path)?;
                    (
                        count(Message::get_count(&conn, &query))?,
                        count(Message::get_conversation_count(&conn, &query))?,
                        count(Message::get_attachment_count(&conn, &query))?,
                    )
                };

                progress.start(0);
                progress.set_message("Running HTML/TXT export…");
                let mode = if copy {
                    AttachmentMode::Copy
                } else {
                    AttachmentMode::Reference
                };
                let mut options: Options = config(mode).into();
                if copy {
                    // The library's default copy mode re-encodes HEIC
                    // photos (and stickers) as JPEG via `sips`. A backup
                    // must hold the originals: a delete removes them
                    // from the Mac. Browsers may not show HEIC inline in
                    // the HTML, but the files are byte-for-byte intact.
                    options.attachment_manager.mode = AttachmentManagerMode::Clone;
                }
                let mut runtime = Config::new(options).map_err(|e| {
                    AppError::Other(format!("failed to initialise export runtime: {e}"))
                })?;
                runtime.resolve_filtered_handles();
                runtime
                    .start()
                    .map_err(|e| AppError::Other(format!("export failed: {e}")))?;
                progress.finish();

                let copy_summary = if copy {
                    let conn = open_read(&db_path)?;
                    Some(verify_runtime_copies(
                        &conn,
                        &db_path,
                        &query,
                        &export_path,
                    )?)
                } else {
                    None
                };
                (
                    messages,
                    attachments,
                    conversations,
                    String::new(),
                    copy_summary,
                )
            }
        };

    // A delete of these messages also removes tapbacks on them that
    // fall outside the filter (so the export doesn't contain them).
    // Save those alongside, so the backup really holds everything the
    // delete would remove.
    if !reaction_ids.is_empty() {
        let conn = open_read(&db_path)?;
        write_extra_tapbacks(&conn, &export_path, &reaction_ids)?;
    }

    // Only attachments the delete plan would touch count, so the
    // fingerprint lines up with the one computed at delete time (the
    // export can also include e.g. "Recently Deleted" messages).
    let planned: HashSet<i64> = planned_attachments.into_iter().collect();
    let copied_ids: Vec<i64> = copy_summary
        .as_ref()
        .map(|c| {
            c.copied_ids
                .iter()
                .copied()
                .filter(|id| planned.contains(id))
                .collect()
        })
        .unwrap_or_default();
    let copied = ContentFingerprint::of(&[], &copied_ids);

    let result = BackupResult {
        message_count,
        attachment_count,
        attachment_bytes_copied: copy_summary.as_ref().map_or(0, |c| c.bytes),
        conversation_count,
        manifest_path,
        export_path: export_label,
        format: format_label,
        includes_attachments: copy_summary.as_ref().is_some_and(|c| c.failed == 0),
        attachment_copy: copy_summary,
    };

    // Only a backup that ran to completion counts toward `run_delete`'s
    // backup gate, and it only vouches for attachments if every copy that
    // could be made was made.
    receipts.record(
        filter_key,
        content,
        copied,
        result.includes_attachments,
        std::path::Path::new(&result.export_path),
    );

    Ok(result)
}
