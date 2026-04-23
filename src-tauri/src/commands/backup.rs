use std::path::PathBuf;

use imessage_database::{
    exporters::{
        config::{AttachmentMode, ExportConfig},
        export_type::ExportType,
        json::run_json_export,
        options::Options,
        pdf::run_pdf_export,
        progress::ProgressReporter,
        runtime::Config,
    },
    tables::{
        messages::message::Message,
        table::get_connection,
    },
};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::core::db_path::default_chat_db_path;
use crate::core::filter::FilterSpec;
use crate::core::progress::TauriProgress;
use crate::error::AppError;

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
    pub export_path: String,
    pub format: String,
}

/// Run a backup with the supplied filter and format to the supplied
/// destination directory. Emits progress events on `backup-progress`.
#[tauri::command]
pub async fn run_backup(app: AppHandle, args: RunBackupArgs) -> Result<BackupResult, AppError> {
    let format = ExportType::from_cli(&args.format)
        .ok_or_else(|| AppError::Other(format!("unsupported format: {}", args.format)))?;

    let db_path = default_chat_db_path()?;
    let destination = PathBuf::from(&args.destination);
    if destination.as_os_str().is_empty() {
        return Err(AppError::Other("destination is required".into()));
    }

    let config = ExportConfig {
        db_path,
        export_path: destination.clone(),
        format,
        query: args.filter.to_query_context()?,
        attachments: if args.copy_attachments {
            AttachmentMode::Copy
        } else {
            AttachmentMode::Reference
        },
        attachment_root: None,
        custom_owner_name: None,
    };

    let progress = TauriProgress::new(app.clone(), BACKUP_PROGRESS_EVENT);
    let format_label = format.to_string();

    let result =
        tokio::task::spawn_blocking(move || -> Result<BackupResult, AppError> {
            match format {
                ExportType::Json => {
                    let summary = run_json_export(&config, &progress)
                        .map_err(|e| AppError::Other(e.to_string()))?;
                    Ok(BackupResult {
                        message_count: summary.message_count,
                        attachment_count: summary.attachment_count,
                        attachment_bytes_copied: summary.attachment_bytes_copied,
                        conversation_count: summary.conversation_count,
                        manifest_path: summary.manifest_path.display().to_string(),
                        export_path: config.export_path.display().to_string(),
                        format: format_label,
                    })
                }
                ExportType::Pdf => {
                    let summary = run_pdf_export(&config, &progress)
                        .map_err(|e| AppError::Other(e.to_string()))?;
                    Ok(BackupResult {
                        message_count: summary.message_count,
                        attachment_count: summary.attachment_count,
                        attachment_bytes_copied: 0,
                        conversation_count: summary.conversation_count,
                        manifest_path: String::new(),
                        export_path: config.export_path.display().to_string(),
                        format: format_label,
                    })
                }
                ExportType::Html | ExportType::Txt => {
                    // HTML / TXT run through the full library runtime. That
                    // path owns its own stderr-based progress bar — migrating
                    // it to `ProgressReporter` is a follow-up; for now we
                    // flip the UI into a generic "running" state and emit a
                    // finish event when start() returns.
                    progress.start(0);
                    progress.set_message("Running HTML/TXT export…");

                    // The library's HTML/TXT runtime doesn't return a summary,
                    // so compute counts from the same filter before running.
                    // Open a scoped readonly connection; drop it before the
                    // library opens its own.
                    let (message_count, conversation_count, attachment_count) = {
                        let conn = get_connection(&config.db_path).map_err(|e| {
                            AppError::Other(format!("failed to open chat.db: {e}"))
                        })?;
                        let messages = Message::get_count(&conn, &config.query)
                            .map_err(|e| AppError::Other(e.to_string()))?
                            .max(0) as u64;
                        let conversations =
                            Message::get_conversation_count(&conn, &config.query)
                                .map_err(|e| AppError::Other(e.to_string()))?
                                .max(0) as u64;
                        let attachments = Message::get_attachment_count(&conn, &config.query)
                            .map_err(|e| AppError::Other(e.to_string()))?
                            .max(0) as u64;
                        (messages, conversations, attachments)
                    };

                    let export_path = config.export_path.clone();
                    let options: Options = config.into();
                    let mut runtime = Config::new(options).map_err(|e| {
                        AppError::Other(format!("failed to initialise export runtime: {e}"))
                    })?;
                    runtime.resolve_filtered_handles();
                    runtime
                        .start()
                        .map_err(|e| AppError::Other(format!("export failed: {e}")))?;
                    progress.finish();
                    Ok(BackupResult {
                        message_count,
                        attachment_count,
                        attachment_bytes_copied: 0,
                        conversation_count,
                        manifest_path: String::new(),
                        export_path: export_path.display().to_string(),
                        format: format_label,
                    })
                }
            }
        })
        .await
        .map_err(|e| AppError::Other(format!("task join error: {e}")))??;

    Ok(result)
}
