use crate::core::db_path::default_chat_db_path;
use crate::core::filter::FilterSpec;
use crate::error::AppError;
use imessage_database::tables::{
    table::get_connection,
    write::{attachment_filter::AttachmentFilter, delete::preview_delete as lib_preview_delete},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupPreview {
    pub message_count: i64,
    /// Total attachment records in the database matching the filter.
    pub attachment_count: u64,
    pub attachment_bytes: u64,
    /// Files physically present on disk right now.
    pub on_disk_count: u64,
    /// Files whose parent UUID directory exists (Messages synced the metadata)
    /// but whose content has not been downloaded to this Mac yet.
    pub not_on_mac_count: u64,
    /// Files referenced in the database with no resolvable local path —
    /// no parent directory, no file, unrecoverable.
    pub missing_count: u64,
    pub has_filters: bool,
}

/// Count messages and attachment storage that match the given filter.
/// Reuses the delete-plan machinery so all filter types (date, chat, handle)
/// apply to both counts without a second query.
#[tauri::command]
pub async fn preview_backup(filter: FilterSpec) -> Result<BackupPreview, AppError> {
    let path = default_chat_db_path()?;
    let conn = get_connection(&path)?;
    let ctx = filter.to_query_context()?;
    let has_filters = ctx.has_filters();

    let plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &path)
        .map_err(|e| AppError::Database(format!("failed to count messages: {e}")))?;

    let mut on_disk_count = 0u64;
    let mut not_on_mac_count = 0u64;
    let mut missing_count = 0u64;

    for p in &plan.attachment_files_on_disk {
        if p.exists() {
            on_disk_count += 1;
        } else if p.parent().is_some_and(|d| d.exists()) {
            // The UUID directory was provisioned by Messages sync but the file
            // content has not been downloaded to this Mac yet.
            not_on_mac_count += 1;
        } else {
            missing_count += 1;
        }
    }

    // Attachments with no resolvable path at all are also missing.
    let unresolvable = plan
        .attachment_rowids
        .len()
        .saturating_sub(plan.attachment_files_on_disk.len()) as u64;
    missing_count += unresolvable;

    Ok(BackupPreview {
        message_count: plan.message_rowids.len() as i64,
        attachment_count: plan.attachment_rowids.len() as u64,
        attachment_bytes: plan.attachment_bytes,
        on_disk_count,
        not_on_mac_count,
        missing_count,
        has_filters,
    })
}
