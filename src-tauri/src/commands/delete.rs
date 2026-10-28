use imessage_database::{
    tables::write::{
        attachment_filter::AttachmentFilter,
        delete::{execute_delete, preview_delete as lib_preview_delete, DeletePlan},
    },
    util::query_context::QueryContext,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::core::attachment_files::{
    attachment_roots, expand_tilde, path_key, remove_attachment_file, RemoveOutcome,
};
use crate::core::db_path::{blocking, default_chat_db_path, open_read, open_write};
use crate::core::filter::FilterSpec;
use crate::core::icloud::{detect_icloud_messages, ICloudState};
use crate::core::messages_app::is_messages_running;
use crate::core::op_lock::OperationLock;
use crate::core::receipts::{BackupReceipts, ContentFingerprint, Coverage};
use crate::core::snapshot::{
    checkpoint_wal, default_snapshot_root, prune_snapshots, snapshot_chat_db, SnapshotKind,
    SNAPSHOTS_TO_KEEP,
};
use crate::core::space::{ensure_space, snapshot_size, SpaceProbe};
use crate::error::AppError;

pub const DELETE_CONFIRMATION_PHRASE: &str = "DELETE";

/// Apply a `DeleteScope` to a freshly-computed plan, zeroing out the parts
/// the caller wants to keep. Pulled out of `run_delete` so the slicing
/// logic is covered by unit tests without needing a real chat.db.
pub(crate) fn apply_scope(plan: &mut DeletePlan, scope: &DeleteScope) -> Result<(), AppError> {
    match scope {
        DeleteScope::MessagesOnly => Err(AppError::Other(
            "Deleting messages while keeping their attachments isn't possible: chat.db's own \
             triggers delete an attachment's record as soon as its last message is gone, which \
             would leave the files untracked. Delete messages together with their attachments, \
             or attachments only."
                .into(),
        )),
        DeleteScope::AttachmentsOnly => {
            // Keep messages; only remove attachment records and files.
            plan.message_rowids.clear();
            Ok(())
        }
        DeleteScope::Both => Ok(()),
    }
}

/// Drop attachments from `plan` that a message outside the plan still links
/// to, and drop on-disk files that an attachment row outside the plan still
/// points at. The library plans every attachment joined to a matched
/// message; without this, deleting one message could take an attachment (or
/// its file) out from under a message the user kept.
///
/// Must run before [`apply_scope`], against the full matched message set.
pub(crate) fn exclude_shared_attachments(
    conn: &Connection,
    plan: &mut DeletePlan,
) -> Result<(), AppError> {
    if plan.attachment_rowids.is_empty() {
        return Ok(());
    }
    let planned_messages: HashSet<i64> = plan.message_rowids.iter().copied().collect();

    let mut shared: HashSet<i64> = HashSet::new();
    for chunk in plan.attachment_rowids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT attachment_id, message_id FROM message_attachment_join \
             WHERE attachment_id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (attachment_id, message_id) = row?;
            if !planned_messages.contains(&message_id) {
                shared.insert(attachment_id);
            }
        }
    }

    let mut stmt =
        conn.prepare("SELECT ROWID, filename, COALESCE(total_bytes, 0) FROM attachment")?;
    let rows: Vec<(i64, Option<String>, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    plan.attachment_rowids.retain(|id| !shared.contains(id));
    let deleting: HashSet<i64> = plan.attachment_rowids.iter().copied().collect();

    let mut bytes_by_row: HashMap<i64, i64> = HashMap::new();
    let mut kept_files: HashSet<String> = HashSet::new();
    for (rowid, filename, bytes) in rows {
        bytes_by_row.insert(rowid, bytes);
        if !deleting.contains(&rowid) {
            if let Some(f) = filename {
                kept_files.insert(path_key(&expand_tilde(&f)));
            }
        }
    }

    for id in &shared {
        let bytes = bytes_by_row.get(id).copied().unwrap_or(0).max(0) as u64;
        plan.attachment_bytes = plan.attachment_bytes.saturating_sub(bytes);
    }
    plan.attachment_files_on_disk
        .retain(|p| !kept_files.contains(&path_key(p)));
    Ok(())
}

/// What a delete means on an iCloud-synced account. chat.db's own delete
/// triggers record every removed message and attachment in
/// `sync_deleted_messages` / `sync_deleted_attachments` — the queue Messages
/// uses to push deletions to iCloud — so the delete is expected to reach the
/// user's other devices the next time Messages syncs.
const ICLOUD_RISK: &str = "Deleting here records each deletion for Messages in iCloud, so when \
    Messages next syncs, these messages and attachments are expected to be deleted from iCloud \
    and your other devices too — not just this Mac.";

/// Extract the target message guid from an `associated_message_guid`
/// (`p:0/<guid>`, `bp:<guid>`, or a bare guid).
pub(crate) fn reaction_target(associated: &str) -> &str {
    if let Some(rest) = associated.strip_prefix("bp:") {
        rest
    } else if let Some(rest) = associated.strip_prefix("p:") {
        rest.split_once('/').map_or(rest, |(_, g)| g)
    } else {
        associated
    }
}

fn is_missing_column(e: &rusqlite::Error) -> bool {
    // Reported as `SqlInputError` or `SqliteFailure` depending on the
    // rusqlite build; the message is the stable part.
    e.to_string().contains("no such column")
}

/// Add tapbacks (and tapback removals) whose target message is being
/// deleted, so they aren't left pointing at nothing. Reactions that carry
/// attachments (sticker reactions) are left alone rather than orphaning
/// their attachment.
pub(crate) fn add_orphaned_reactions(
    conn: &Connection,
    plan: &mut DeletePlan,
) -> Result<Vec<i64>, AppError> {
    if plan.message_rowids.is_empty() {
        return Ok(Vec::new());
    }
    let mut guids: HashSet<String> = HashSet::new();
    for chunk in plan.message_rowids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT guid FROM message WHERE ROWID IN ({placeholders})"
        ))?;
        for g in stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))? {
            guids.insert(g?);
        }
    }

    let stmt = conn.prepare(
        "SELECT ROWID, associated_message_guid FROM message \
         WHERE associated_message_guid IS NOT NULL \
           AND associated_message_type BETWEEN 2000 AND 3999 \
           AND COALESCE(cache_has_attachments, 0) = 0",
    );
    let mut stmt = match stmt {
        Ok(s) => s,
        // Very old (or test) schemas don't track reactions this way.
        Err(e) if is_missing_column(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let planned: HashSet<i64> = plan.message_rowids.iter().copied().collect();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut added = Vec::new();
    for row in rows {
        let (rowid, associated) = row?;
        if !planned.contains(&rowid) && guids.contains(reaction_target(&associated)) {
            plan.message_rowids.push(rowid);
            added.push(rowid);
        }
    }
    plan.message_rowids.sort_unstable();
    plan.message_rowids.dedup();
    Ok(added)
}

/// Messages linked to any of `attachment_ids`.
fn messages_linked_to(conn: &Connection, attachment_ids: &[i64]) -> Result<Vec<i64>, AppError> {
    let mut out = Vec::new();
    for chunk in attachment_ids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT message_id FROM message_attachment_join \
             WHERE attachment_id IN ({placeholders})"
        ))?;
        for id in stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))? {
            out.push(id?);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// After an attachments-only delete, messages that no longer have any
/// attachment still carry `cache_has_attachments = 1`, which makes Messages
/// and exporters look for attachments that are gone. Best-effort: runs in
/// its own transaction after the delete committed, and a failure only means
/// the flag stays stale.
pub(crate) fn clear_stale_attachment_flags(
    conn: &mut Connection,
    message_ids: &[i64],
) -> Result<u64, AppError> {
    if message_ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut updated = 0u64;
    for chunk in message_ids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE message SET cache_has_attachments = 0 \
             WHERE ROWID IN ({placeholders}) \
               AND NOT EXISTS (SELECT 1 FROM message_attachment_join j \
                               WHERE j.message_id = message.ROWID)"
        );
        match tx.execute(&sql, params_from_iter(chunk.iter())) {
            Ok(n) => updated += n as u64,
            Err(e) if is_missing_column(&e) => return Ok(0),
            Err(e) => return Err(e.into()),
        }
    }
    tx.commit()?;
    Ok(updated)
}

/// Planned attachments whose file isn't on this Mac (typically offloaded to
/// iCloud). No backup can include them, and deleting their records removes
/// the only way to download them again.
pub(crate) fn count_not_on_mac(conn: &Connection, plan: &DeletePlan) -> Result<u64, AppError> {
    let mut n = 0u64;
    for chunk in plan.attachment_rowids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT filename FROM attachment WHERE ROWID IN ({placeholders})"
        ))?;
        for f in stmt.query_map(params_from_iter(chunk.iter()), |r| {
            r.get::<_, Option<String>>(0)
        })? {
            if let Some(f) = f? {
                if !expand_tilde(&f).exists() {
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

/// The full (unscoped) plan for `ctx`, shared by the preview and the delete
/// so the counts the user confirms are the counts that get deleted.
/// What a filter matches, in the form a backup records and a delete
/// checks: the filter's messages and attachments plus the tapbacks (outside
/// the filter) that deleting those messages would also remove.
pub(crate) struct FilterContent {
    /// The library plan with the tapbacks added; shared attachments not yet
    /// excluded.
    pub plan: DeletePlan,
    pub reaction_ids: Vec<i64>,
    /// Everything that would be deleted or backed up.
    pub content: ContentFingerprint,
    /// Which of the matched attachments have their file on this Mac right
    /// now. A backup only protects the files that were present (and copied)
    /// when it ran; anything downloaded afterwards changes this.
    pub present: ContentFingerprint,
}

/// Attachment ids (of `ids`) whose file is on this Mac.
pub(crate) fn present_attachments(conn: &Connection, ids: &[i64]) -> Result<Vec<i64>, AppError> {
    let mut out = Vec::new();
    for chunk in ids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT ROWID, filename FROM attachment WHERE ROWID IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (id, f) = row?;
            if f.is_some_and(|f| expand_tilde(&f).exists()) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// The library collects attachments per chunk of 800 messages with a
/// per-chunk `DISTINCT`, so an attachment linked from messages in two
/// chunks is listed (and its size counted) twice. De-duplicate the ids and
/// files and recompute the total from the database.
fn dedup_attachments(conn: &Connection, plan: &mut DeletePlan) -> Result<(), AppError> {
    let before = plan.attachment_rowids.len();
    plan.attachment_rowids.sort_unstable();
    plan.attachment_rowids.dedup();
    let mut seen: HashSet<String> = HashSet::new();
    plan.attachment_files_on_disk
        .retain(|p| seen.insert(path_key(p)));
    if plan.attachment_rowids.len() == before {
        return Ok(());
    }
    let mut total: u64 = 0;
    for chunk in plan.attachment_rowids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT COALESCE(total_bytes, 0) FROM attachment WHERE ROWID IN ({placeholders})"
        ))?;
        for b in stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))? {
            total = total.saturating_add(b?.max(0) as u64);
        }
    }
    plan.attachment_bytes = total;
    Ok(())
}

pub(crate) fn filter_content(
    conn: &Connection,
    ctx: &QueryContext,
    db_path: &std::path::Path,
) -> Result<FilterContent, AppError> {
    let mut plan = lib_preview_delete(conn, ctx, &AttachmentFilter::any(), db_path)
        .map_err(|e| AppError::Database(format!("failed to compute delete plan: {e}")))?;
    dedup_attachments(conn, &mut plan)?;
    let reaction_ids = add_orphaned_reactions(conn, &mut plan)?;
    let content = ContentFingerprint::of(&plan.message_rowids, &plan.attachment_rowids);
    let present = ContentFingerprint::of(&[], &present_attachments(conn, &plan.attachment_rowids)?);
    Ok(FilterContent {
        plan,
        reaction_ids,
        content,
        present,
    })
}

pub(crate) struct BuiltPlan {
    pub plan: DeletePlan,
    /// Tapbacks outside the filter added because their target message is
    /// being deleted.
    pub reactions: u64,
    pub content: ContentFingerprint,
    pub present: ContentFingerprint,
}

pub(crate) fn build_plan(
    conn: &Connection,
    ctx: &QueryContext,
    db_path: &std::path::Path,
) -> Result<BuiltPlan, AppError> {
    let FilterContent {
        mut plan,
        reaction_ids,
        content,
        present,
    } = filter_content(conn, ctx, db_path)?;
    exclude_shared_attachments(conn, &mut plan)?;
    Ok(BuiltPlan {
        plan,
        reactions: reaction_ids.len() as u64,
        content,
        present,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePreview {
    pub message_count: u64,
    pub attachment_count: u64,
    pub attachment_bytes: u64,
    pub on_disk_file_count: u64,
    /// Matched attachments whose file isn't on this Mac.
    pub not_on_mac_count: u64,
    /// Of `message_count`: tapbacks outside the filter whose target message
    /// is being deleted (so they'd otherwise point at nothing).
    pub reaction_count: u64,
}

/// Compute what a delete with the given filter would remove. Read-only.
#[tauri::command]
pub async fn preview_delete(filter: FilterSpec) -> Result<DeletePreview, AppError> {
    let db_path = default_chat_db_path()?;
    blocking(move || preview_delete_now(&db_path, &filter)).await
}

pub(crate) fn preview_delete_now(
    db_path: &std::path::Path,
    filter: &FilterSpec,
) -> Result<DeletePreview, AppError> {
    let ctx = filter.to_query_context()?;
    let conn = open_read(db_path)?;
    let BuiltPlan {
        plan,
        reactions: reaction_count,
        ..
    } = build_plan(&conn, &ctx, db_path)?;
    Ok(DeletePreview {
        reaction_count,
        message_count: plan.message_rowids.len() as u64,
        attachment_count: plan.attachment_rowids.len() as u64,
        attachment_bytes: plan.attachment_bytes,
        on_disk_file_count: plan.attachment_files_on_disk.len() as u64,
        not_on_mac_count: count_not_on_mac(&conn, &plan)?,
    })
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum DeleteScope {
    /// Delete both messages and their attachment records/files (default).
    #[default]
    Both,
    /// No longer supported (always refused): chat.db's triggers delete an
    /// attachment's record when its last message link goes, so attachments
    /// can't be kept without their messages. Kept on the wire so an old
    /// caller gets a clear error rather than a parse failure.
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
    /// Required when no backup with exactly this filter ran in this session
    /// (or, when attachment files will be deleted, when that backup didn't
    /// copy all of them). Whether a backup ran is checked against the
    /// backend's own receipts — the frontend can't assert it.
    #[serde(default)]
    pub acknowledge_skip_backup: bool,
    /// Optional override for where snapshots are stored. Defaults to
    /// `<app data>/snapshots/delete/<timestamp>/`.
    pub snapshot_root: Option<String>,
    /// Controls which parts of the matched records are removed.
    #[serde(default)]
    pub delete_scope: DeleteScope,
    /// Must be `true` unless Messages in iCloud is positively detected as
    /// disabled.
    #[serde(default)]
    pub acknowledge_icloud_sync: bool,
    /// Required when attachment records will be deleted and some of their
    /// files aren't on this Mac: those can't be backed up and become
    /// unrecoverable.
    #[serde(default)]
    pub acknowledge_not_on_mac: bool,
    /// Messages the user was shown and confirmed (0 when the scope keeps
    /// messages). When set, the delete refuses if the plan differs — e.g.
    /// the filter changed after the preview was computed.
    pub expected_message_count: Option<u64>,
    /// Attachments the user was shown and confirmed (0 when the scope keeps
    /// attachments). Checked like `expected_message_count`.
    pub expected_attachment_count: Option<u64>,
}

/// Refuse when the scoped plan doesn't match what the user confirmed.
pub(crate) fn check_expected_counts(
    plan: &DeletePlan,
    expected_messages: Option<u64>,
    expected_attachments: Option<u64>,
) -> Result<(), AppError> {
    let messages = plan.message_rowids.len() as u64;
    let attachments = plan.attachment_rowids.len() as u64;
    let mismatch = expected_messages.is_some_and(|n| n != messages)
        || expected_attachments.is_some_and(|n| n != attachments);
    if mismatch {
        return Err(AppError::Other(format!(
            "What would be deleted changed since the preview (now {messages} messages and \
             {attachments} attachments). Nothing was deleted — review the preview and try again."
        )));
    }
    Ok(())
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
    /// Files left alone because they weren't regular files inside the
    /// Messages attachment folders.
    pub on_disk_files_skipped: u64,
    pub backup_verified: bool,
}

/// Returns whether a matching backup exists, or errors if none does and
/// the caller didn't explicitly opt out. The snapshot only covers chat.db,
/// so when attachment files will be unlinked the backup must have copied
/// them.
pub(crate) fn check_backup_gate(
    coverage: &Coverage,
    scope: &DeleteScope,
    acknowledge_skip_backup: bool,
) -> Result<bool, AppError> {
    let deletes_files = !matches!(scope, DeleteScope::MessagesOnly);
    let verified = coverage.covered && (coverage.with_attachments || !deletes_files);
    if verified || acknowledge_skip_backup {
        return Ok(verified);
    }
    let msg = if coverage.covered {
        "The backup of these messages didn't copy every attachment file, and this delete \
         removes them permanently (the safety snapshot only covers chat.db). Re-run the backup \
         with attachments, or pass acknowledgeSkipBackup=true to proceed anyway."
    } else {
        "No backup covers exactly the messages this delete removes (new messages may have \
         arrived since the last one). Run a backup first, or pass acknowledgeSkipBackup=true \
         to proceed without one."
    };
    Err(AppError::Other(msg.into()))
}

/// Whether completed backups cover what `filter` currently matches. Lets
/// the UI show the real state (including backups from earlier sessions)
/// instead of guessing from the last backup it ran.
#[tauri::command]
pub async fn backup_coverage(
    receipts: State<'_, Arc<BackupReceipts>>,
    filter: FilterSpec,
) -> Result<Coverage, AppError> {
    let db_path = default_chat_db_path()?;
    let receipts = Arc::clone(&receipts);
    blocking(move || coverage_now(&db_path, &receipts, &filter)).await
}

pub(crate) fn coverage_now(
    db_path: &std::path::Path,
    receipts: &BackupReceipts,
    filter: &FilterSpec,
) -> Result<Coverage, AppError> {
    let ctx = filter.to_query_context()?;
    let conn = open_read(db_path)?;
    let fc = filter_content(&conn, &ctx, db_path)?;
    Ok(receipts.coverage(&filter.key(), &fc.content, &fc.present))
}

fn ensure_messages_not_running(probe: &SafetyProbe) -> Result<(), AppError> {
    if (probe.messages_running)() {
        return Err(AppError::Other(
            "Quit Messages.app before running a delete — it holds a write lock on chat.db.".into(),
        ));
    }
    Ok(())
}

/// Everything `run_delete` works out from chat.db before deciding whether
/// to go ahead.
struct Prepared {
    /// The scoped plan.
    plan: DeletePlan,
    scope: DeleteScope,
    /// What the filter matches, and which attachment files are present,
    /// for the backup gate.
    content: ContentFingerprint,
    present: ContentFingerprint,
    /// Messages whose attachment flag may need clearing afterwards.
    flag_candidates: Vec<i64>,
    not_on_mac: u64,
}

fn prepare_delete(
    db_path: &std::path::Path,
    ctx: &QueryContext,
    scope: &DeleteScope,
    (expected_messages, expected_attachments): (Option<u64>, Option<u64>),
) -> Result<Prepared, AppError> {
    // Build the delete plan against a read-only connection — even though
    // we'll execute it against a writable one, there's no reason to hold
    // two writers, and the read-only path is more conservative.
    let conn = open_read(db_path)?;
    let BuiltPlan {
        mut plan,
        content,
        present,
        ..
    } = build_plan(&conn, ctx, db_path)?;

    apply_scope(&mut plan, scope)?;

    // Gate 3: the plan must match what the user confirmed.
    check_expected_counts(&plan, expected_messages, expected_attachments)?;

    let not_on_mac = count_not_on_mac(&conn, &plan)?;
    let flag_candidates = if matches!(scope, DeleteScope::AttachmentsOnly) {
        messages_linked_to(&conn, &plan.attachment_rowids)?
    } else {
        Vec::new()
    };
    Ok(Prepared {
        plan,
        scope: *scope,
        content,
        present,
        flag_candidates,
        not_on_mac,
    })
}

/// Execute a delete. Enforces the full safety contract: typed
/// confirmation, backup (or explicit skip), Messages.app-not-running,
/// iCloud acknowledgement, a plan matching what the user confirmed,
/// mandatory snapshot with a WAL checkpoint, then a transactional delete
/// against a writable connection. Attachment files on disk are unlinked
/// **after** the SQL transaction commits; individual unlink failures are
/// counted but do not roll back.
/// Host state a delete depends on. Injectable so end-to-end tests don't
/// depend on whether Messages happens to be running on the test machine.
pub(crate) struct SafetyProbe {
    pub messages_running: fn() -> bool,
    pub icloud: fn() -> ICloudState,
    pub available_space: SpaceProbe,
}

impl SafetyProbe {
    pub fn system() -> Self {
        Self {
            // We err on the side of refusing when we can't tell (pgrep
            // unavailable → treat as "could be running").
            messages_running: || is_messages_running().unwrap_or(true),
            icloud: || detect_icloud_messages().unwrap_or(ICloudState::Unknown),
            available_space: crate::core::space::system_available,
        }
    }
}

#[tauri::command]
pub async fn run_delete(
    app: AppHandle,
    receipts: State<'_, Arc<BackupReceipts>>,
    lock: State<'_, OperationLock>,
    args: RunDeleteArgs,
) -> Result<DeleteResult, AppError> {
    let _guard = lock.try_acquire("a delete")?;
    let db_path = default_chat_db_path()?;
    let (snapshot_root, managed_root) = match args.snapshot_root.as_deref() {
        Some(p) if !p.is_empty() => (
            crate::core::paths::validate_user_path(p, "snapshot root")?,
            false,
        ),
        _ => (default_snapshot_root(&app, SnapshotKind::Delete)?, true),
    };
    let receipts = Arc::clone(&receipts);
    blocking(move || {
        delete_now(
            &db_path,
            &receipts,
            args,
            &snapshot_root,
            managed_root,
            &SafetyProbe::system(),
        )
    })
    .await
}

/// The whole delete, gates included, as a plain function (see
/// [`run_delete`]).
pub(crate) fn delete_now(
    db_path: &std::path::Path,
    receipts: &BackupReceipts,
    args: RunDeleteArgs,
    snapshot_root: &std::path::Path,
    managed_root: bool,
    probe: &SafetyProbe,
) -> Result<DeleteResult, AppError> {
    // Gate 1: typed confirmation.
    if args.confirmation_phrase != DELETE_CONFIRMATION_PHRASE {
        return Err(AppError::Other(format!(
            "confirmation phrase must be exactly '{DELETE_CONFIRMATION_PHRASE}'"
        )));
    }

    // Gate 2: Messages.app must be quit.
    ensure_messages_not_running(probe)?;

    // Gate 2a: Messages in iCloud. Detection is best-effort, so anything
    // other than a positive "disabled" needs an explicit acknowledgement.
    match (probe.icloud)() {
        ICloudState::Disabled => {}
        ICloudState::Enabled if !args.acknowledge_icloud_sync => {
            return Err(AppError::Other(format!(
                "Messages in iCloud is enabled. {ICLOUD_RISK} Acknowledge the iCloud warning \
                 in the UI (or pass acknowledgeIcloudSync=true) before retrying."
            )));
        }
        ICloudState::Unknown if !args.acknowledge_icloud_sync => {
            return Err(AppError::Other(format!(
                "Couldn't determine whether Messages in iCloud is enabled. If it is: \
                 {ICLOUD_RISK} Acknowledge the iCloud warning in the UI (or pass \
                 acknowledgeIcloudSync=true) before retrying."
            )));
        }
        _ => {}
    }

    let ctx = args.filter.to_query_context()?;
    let filter_key = args.filter.key();
    let scope = args.delete_scope;
    let expected = (args.expected_message_count, args.expected_attachment_count);
    let prepared = prepare_delete(db_path, &ctx, &scope, expected)?;

    // Gate 3b: backup gate, against the exact content being deleted — a
    // backup of the same filter taken before newer messages arrived doesn't
    // cover them.
    let coverage = receipts.coverage(&filter_key, &prepared.content, &prepared.present);
    let backup_verified =
        check_backup_gate(&coverage, &prepared.scope, args.acknowledge_skip_backup)?;

    // Gate 3c: attachments that exist only in iCloud can't be backed up
    // and become unrecoverable once their records are gone.
    if prepared.not_on_mac > 0 && !args.acknowledge_not_on_mac {
        return Err(AppError::Other(format!(
            "{} attachments in this delete aren't downloaded to this Mac, so no backup can \
             include them and they can't be recovered afterwards. Download them in Messages \
             first, or pass acknowledgeNotOnMac=true to delete them anyway.",
            prepared.not_on_mac
        )));
    }

    let Prepared {
        plan,
        flag_candidates,
        ..
    } = prepared;

    // Gate 4: mandatory WAL checkpoint + snapshot — refused up front if
    // the snapshot folder's volume can't hold it.
    ensure_space(
        snapshot_root,
        snapshot_size(db_path),
        "the safety snapshot",
        probe.available_space,
    )?;
    checkpoint_wal(db_path)?;
    let snapshot = snapshot_chat_db(db_path, snapshot_root)?;

    // Everything that can still fail before the delete commits. If any of
    // it does, nothing changed, so the snapshot just taken is redundant:
    // remove it rather than let it push an older (real) rollback out.
    let committed = (|| {
        // Re-check right before writing: the snapshot of a large chat.db
        // can take long enough for the user to reopen Messages.
        ensure_messages_not_running(probe)?;
        let mut write_conn = open_write(db_path)?;
        let report = execute_delete(&mut write_conn, &plan).map_err(|e| {
            AppError::Database(format!(
                "delete transaction failed (rolled back; no changes made): {e}"
            ))
        })?;
        Ok::<_, AppError>((write_conn, report))
    })();
    let (mut write_conn, report) = match committed {
        Ok(c) => c,
        Err(e) => {
            if let Err(re) = std::fs::remove_dir_all(&snapshot.dir) {
                eprintln!("delete: couldn't remove unused snapshot: {re}");
            }
            return Err(e);
        }
    };
    if managed_root {
        // Only after a successful delete, and only in the folder we own; a
        // caller-chosen root is theirs.
        prune_snapshots(snapshot_root, SNAPSHOTS_TO_KEEP, &snapshot.dir);
    }
    if let Err(e) = clear_stale_attachment_flags(&mut write_conn, &flag_candidates) {
        eprintln!("delete: couldn't clear attachment flags on kept messages: {e:?}");
    }
    // Release the writable handle immediately so Messages.app can
    // reopen.
    drop(write_conn);

    // Unlink attachment files from disk. Failures are non-fatal — the
    // DB is the source of truth, so a stale file on disk is a minor
    // janitor issue, not a data-integrity event. Paths come from
    // chat.db, so only regular files inside the Messages attachment
    // folders are ever removed.
    let roots = attachment_roots();
    let mut removed = 0u64;
    let mut failed = 0u64;
    let mut skipped = 0u64;
    for p in &plan.attachment_files_on_disk {
        match remove_attachment_file(p, &roots, "delete") {
            RemoveOutcome::Removed => removed += 1,
            RemoveOutcome::Missing => {}
            RemoveOutcome::Refused => skipped += 1,
            RemoveOutcome::Failed => failed += 1,
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
        on_disk_files_skipped: skipped,
        backup_verified,
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
                is_from_me INTEGER DEFAULT 0,
                ck_record_id TEXT,
                balloon_bundle_id TEXT
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
                hide_attachment INTEGER DEFAULT 0,
                guid TEXT,
                ck_record_id TEXT
            );
            CREATE TABLE chat_message_join (
                chat_id INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                PRIMARY KEY (chat_id, message_id)
            );
            CREATE TABLE message_attachment_join (
                message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE,
                attachment_id INTEGER REFERENCES attachment (ROWID) ON DELETE CASCADE,
                UNIQUE (message_id, attachment_id)
            );
            CREATE TABLE chat_handle_join (
                chat_id INTEGER NOT NULL,
                handle_id INTEGER NOT NULL,
                PRIMARY KEY (chat_id, handle_id)
            );
            -- Apple's own bookkeeping and triggers (copied from a real
            -- chat.db). They change what a delete actually does, so tests
            -- must run with them.
            CREATE TABLE sync_deleted_messages (guid TEXT, recordID TEXT);
            CREATE TABLE sync_deleted_attachments (guid TEXT, recordID TEXT);
            CREATE TABLE deleted_messages (guid TEXT);
            CREATE TRIGGER after_delete_on_message_attachment_join
              AFTER DELETE ON message_attachment_join BEGIN
                DELETE FROM attachment WHERE attachment.ROWID = OLD.attachment_id
                  AND (SELECT 1 FROM message_attachment_join
                       WHERE attachment_id = OLD.attachment_id LIMIT 1) IS NULL;
              END;
            CREATE TRIGGER before_delete_on_attachment BEFORE DELETE ON attachment BEGIN
                SELECT before_delete_attachment_path(OLD.ROWID, OLD.guid);
              END;
            CREATE TRIGGER after_delete_on_attachment AFTER DELETE ON attachment BEGIN
                SELECT delete_attachment_path(OLD.filename);
              END;
            CREATE TRIGGER add_to_sync_deleted_attachments AFTER DELETE ON attachment BEGIN
                INSERT INTO sync_deleted_attachments (guid, recordID)
                  VALUES (OLD.guid, OLD.ck_record_id);
              END;
            CREATE TRIGGER add_to_sync_deleted_messages AFTER DELETE ON message BEGIN
                INSERT INTO sync_deleted_messages (guid, recordID)
                  VALUES (OLD.guid, OLD.ck_record_id);
              END;
            CREATE TRIGGER add_to_deleted_messages AFTER DELETE ON message BEGIN
                INSERT INTO deleted_messages (guid) VALUES (OLD.guid);
              END;
            CREATE TRIGGER after_delete_on_message_plugin AFTER DELETE ON message
              WHEN OLD.balloon_bundle_id IS NOT NULL BEGIN
                SELECT after_delete_message_plugin(OLD.ROWID, OLD.guid);
              END;",
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

    /// How the app opens chat.db for writing: foreign keys on and Apple's
    /// trigger functions stubbed, so cascades and triggers behave for real.
    fn writable(db_path: &std::path::Path) -> Connection {
        crate::core::db_path::open_write(db_path).unwrap()
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
    fn messages_only_scope_is_refused() {
        let mut plan = DeletePlan {
            message_rowids: vec![1, 2],
            attachment_rowids: vec![10],
            ..Default::default()
        };
        let err = apply_scope(&mut plan, &DeleteScope::MessagesOnly).unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("isn't possible")));
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
        apply_scope(&mut plan, &DeleteScope::AttachmentsOnly).unwrap();
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
        apply_scope(&mut plan, &DeleteScope::Both).unwrap();
        assert_eq!(plan.message_rowids, original.message_rowids);
        assert_eq!(plan.attachment_rowids, original.attachment_rowids);
        assert_eq!(plan.attachment_bytes, original.attachment_bytes);
    }

    #[test]
    fn backup_gate_requires_matching_backup_with_attachments_when_deleting_files() {
        let none = Coverage::default();
        let refs_only = Coverage {
            covered: true,
            with_attachments: false,
            export_path: None,
        };
        let full = Coverage {
            covered: true,
            with_attachments: true,
            export_path: None,
        };

        assert!(check_backup_gate(&none, &DeleteScope::Both, false).is_err());
        assert!(!check_backup_gate(&none, &DeleteScope::Both, true).unwrap());

        // A references-only backup covers a messages-only delete…
        assert!(check_backup_gate(&refs_only, &DeleteScope::MessagesOnly, false).unwrap());
        // …but not one that unlinks attachment files.
        let err = check_backup_gate(&refs_only, &DeleteScope::AttachmentsOnly, false).unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("didn't copy every attachment")));

        assert!(check_backup_gate(&full, &DeleteScope::Both, false).unwrap());
    }

    #[test]
    fn backup_stops_covering_once_new_messages_match_the_filter() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        let backup_dir = tempfile::TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let filter = FilterSpec::default();
        let ctx = filter.to_query_context().unwrap();

        // Back up: attachment 1's file is on the Mac and gets copied.
        let fc = filter_content(&conn, &ctx, &db_path).unwrap();
        let copied = ContentFingerprint::of(&[], &[1]);
        receipts.record(filter.key(), fc.content, copied, true, backup_dir.path());
        let p = build_plan(&conn, &ctx, &db_path).unwrap();
        assert!(
            receipts
                .coverage(&filter.key(), &p.content, &p.present)
                .with_attachments
        );

        // A message arrives after the backup; same filter, new content.
        conn.execute_batch(
            "INSERT INTO message (ROWID, guid, date) VALUES (3, 'g3', 0);
             INSERT INTO chat_message_join VALUES (1, 3);",
        )
        .unwrap();
        let p = build_plan(&conn, &ctx, &db_path).unwrap();
        assert!(
            !receipts
                .coverage(&filter.key(), &p.content, &p.present)
                .covered
        );
    }

    #[test]
    fn attachment_downloaded_after_backup_is_not_covered() {
        let (tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        let offloaded = tmp.path().join("offloaded.jpg");
        conn.execute(
            "INSERT INTO attachment (ROWID, filename) VALUES (2, ?1)",
            params![offloaded.to_str().unwrap()],
        )
        .unwrap();
        conn.execute("INSERT INTO message_attachment_join VALUES (1, 2)", [])
            .unwrap();
        let backup_dir = tempfile::TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let filter = FilterSpec::default();
        let ctx = filter.to_query_context().unwrap();

        // Backup copies attachment 1; attachment 2 is still in iCloud.
        let fc = filter_content(&conn, &ctx, &db_path).unwrap();
        receipts.record(
            filter.key(),
            fc.content,
            ContentFingerprint::of(&[], &[1]),
            true,
            backup_dir.path(),
        );
        let p = build_plan(&conn, &ctx, &db_path).unwrap();
        assert!(
            receipts
                .coverage(&filter.key(), &p.content, &p.present)
                .with_attachments
        );

        // The user downloads it; the backup doesn't have it.
        std::fs::write(&offloaded, b"now here").unwrap();
        let p = build_plan(&conn, &ctx, &db_path).unwrap();
        let c = receipts.coverage(&filter.key(), &p.content, &p.present);
        assert!(c.covered && !c.with_attachments);
    }

    #[test]
    fn late_tapback_on_an_in_range_message_breaks_coverage() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        conn.execute_batch(
            "ALTER TABLE message ADD COLUMN associated_message_guid TEXT;
             ALTER TABLE message ADD COLUMN associated_message_type INTEGER DEFAULT 0;
             ALTER TABLE message ADD COLUMN cache_has_attachments INTEGER DEFAULT 0;
             INSERT INTO chat (ROWID, chat_identifier) VALUES (2, 'other');",
        )
        .unwrap();
        let mut ctx = QueryContext::default();
        ctx.set_selected_chat_ids(std::collections::BTreeSet::from([1]));
        let at_backup = filter_content(&conn, &ctx, &db_path).unwrap().content;

        // Someone reacts later, from a chat outside the filter.
        conn.execute_batch(
            "INSERT INTO message (ROWID, guid, date, associated_message_guid, associated_message_type)
               VALUES (10, 'r1', 0, 'p:0/g1', 2000);
             INSERT INTO chat_message_join VALUES (2, 10);",
        )
        .unwrap();
        let now = filter_content(&conn, &ctx, &db_path).unwrap();
        assert_eq!(now.reaction_ids, vec![10]);
        assert_ne!(now.content, at_backup);
    }

    #[test]
    fn shared_attachments_and_files_are_excluded_from_plan() {
        let (tmp, db_path, att1) = fresh_fixture();
        let conn = writable(&db_path);
        // Message 3 lives in another chat and is NOT being deleted.
        let shared_file = tmp.path().join("shared.jpg");
        std::fs::write(&shared_file, b"x").unwrap();
        conn.execute_batch(
            "INSERT INTO chat (ROWID, chat_identifier) VALUES (2, 'other');
             INSERT INTO message (ROWID, guid, handle_id, date) VALUES (3, 'g3', 1, 0);
             INSERT INTO chat_message_join VALUES (2, 3);
             -- Attachment 2 is linked from a deleted message and a kept one.
             INSERT INTO attachment (ROWID, filename, total_bytes) VALUES (2, '/nowhere/a2', 100);
             INSERT INTO message_attachment_join VALUES (1, 2), (3, 2);",
        )
        .unwrap();
        // Attachments 3 (deleted message) and 4 (kept message) share a file.
        conn.execute(
            "INSERT INTO attachment (ROWID, filename, total_bytes) VALUES (3, ?1, 7), (4, ?1, 7)",
            params![shared_file.to_str().unwrap()],
        )
        .unwrap();
        conn.execute_batch("INSERT INTO message_attachment_join VALUES (1, 3), (3, 4);")
            .unwrap();

        let mut ctx = QueryContext::default();
        ctx.set_selected_chat_ids(std::collections::BTreeSet::from([1]));
        let mut plan = lib_preview_delete(&conn, &ctx, &AttachmentFilter::any(), &db_path).unwrap();
        let mut planned = plan.attachment_rowids.clone();
        planned.sort();
        assert_eq!(planned, vec![1, 2, 3]);

        exclude_shared_attachments(&conn, &mut plan).unwrap();
        let mut kept = plan.attachment_rowids.clone();
        kept.sort();
        // Attachment 2 is still linked to message 3, so it stays.
        assert_eq!(kept, vec![1, 3]);
        assert_eq!(plan.attachment_bytes, 15 + 7);
        // Row 3 goes, but its file is still used by row 4.
        assert!(plan.attachment_files_on_disk.contains(&att1));
        assert!(!plan.attachment_files_on_disk.contains(&shared_file));
    }

    #[test]
    fn attachments_only_delete_clears_stale_flags() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let mut conn = writable(&db_path);
        conn.execute_batch(
            "ALTER TABLE message ADD COLUMN cache_has_attachments INTEGER DEFAULT 0;
             UPDATE message SET cache_has_attachments = 1 WHERE ROWID = 2;
             -- Message 1 has two attachments; only one is deleted.
             INSERT INTO attachment (ROWID, filename) VALUES (2, '/x/a'), (3, '/x/b');
             INSERT INTO message_attachment_join VALUES (1, 2), (1, 3);
             UPDATE message SET cache_has_attachments = 1 WHERE ROWID = 1;",
        )
        .unwrap();
        let plan = DeletePlan {
            attachment_rowids: vec![1, 2],
            ..Default::default()
        };
        let candidates = messages_linked_to(&conn, &plan.attachment_rowids).unwrap();
        assert_eq!(candidates, vec![1, 2]);
        execute_delete(&mut conn, &plan).unwrap();

        assert_eq!(
            clear_stale_attachment_flags(&mut conn, &candidates).unwrap(),
            1
        );
        let flag = |id: i64| -> i64 {
            conn.query_row(
                "SELECT cache_has_attachments FROM message WHERE ROWID = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(flag(2), 0, "lost its only attachment");
        assert_eq!(flag(1), 1, "still has attachment 3");
    }

    #[test]
    fn reaction_targets_are_parsed() {
        assert_eq!(reaction_target("p:0/ABC-123"), "ABC-123");
        assert_eq!(reaction_target("p:12/ABC"), "ABC");
        assert_eq!(reaction_target("bp:ABC"), "ABC");
        assert_eq!(reaction_target("ABC"), "ABC");
    }

    #[test]
    fn tapbacks_on_deleted_messages_are_deleted_too() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        conn.execute_batch(
            "ALTER TABLE message ADD COLUMN associated_message_guid TEXT;
             ALTER TABLE message ADD COLUMN associated_message_type INTEGER DEFAULT 0;
             ALTER TABLE message ADD COLUMN cache_has_attachments INTEGER DEFAULT 0;
             INSERT INTO chat (ROWID, chat_identifier) VALUES (2, 'other');
             -- Tapback in another chat on message g1 (being deleted): goes.
             INSERT INTO message (ROWID, guid, date, associated_message_guid, associated_message_type)
               VALUES (10, 'r1', 0, 'p:0/g1', 2000);
             -- Sticker reaction with an attachment on g1: stays.
             INSERT INTO message (ROWID, guid, date, associated_message_guid, associated_message_type, cache_has_attachments)
               VALUES (11, 'r2', 0, 'p:0/g1', 2007, 1);
             -- Tapback on a message that is kept: stays.
             INSERT INTO message (ROWID, guid, date) VALUES (12, 'kept', 0);
             INSERT INTO message (ROWID, guid, date, associated_message_guid, associated_message_type)
               VALUES (13, 'r3', 0, 'p:0/kept', 2001);
             -- A reply (not a reaction) to g1: stays.
             INSERT INTO message (ROWID, guid, date, associated_message_guid, associated_message_type)
               VALUES (14, 'r4', 0, 'g1', 0);
             INSERT INTO chat_message_join VALUES (2, 10), (2, 11), (2, 12), (2, 13), (2, 14);",
        )
        .unwrap();

        let mut ctx = QueryContext::default();
        ctx.set_selected_chat_ids(std::collections::BTreeSet::from([1]));
        let BuiltPlan {
            plan, reactions, ..
        } = build_plan(&conn, &ctx, &db_path).unwrap();
        assert_eq!(plan.message_rowids, vec![1, 2, 10]);
        assert_eq!(reactions, 1);
    }

    #[test]
    fn reactions_are_skipped_on_schemas_without_them() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        let plan = build_plan(&conn, &QueryContext::default(), &db_path)
            .unwrap()
            .plan;
        assert_eq!(plan.message_rowids, vec![1, 2]);
    }

    #[test]
    fn not_on_mac_counts_missing_files_only() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
        conn.execute_batch(
            "INSERT INTO attachment (ROWID, filename) VALUES (2, '/nowhere/offloaded.jpg'), (3, NULL);
             INSERT INTO message_attachment_join VALUES (1, 2), (1, 3);",
        )
        .unwrap();
        let plan = build_plan(&conn, &QueryContext::default(), &db_path)
            .unwrap()
            .plan;
        // Row 1's file exists, row 2's doesn't, row 3 has no file at all.
        assert_eq!(count_not_on_mac(&conn, &plan).unwrap(), 1);
    }

    #[test]
    fn expected_counts_must_match_scoped_plan() {
        let plan = DeletePlan {
            message_rowids: vec![1, 2],
            attachment_rowids: vec![10],
            ..Default::default()
        };
        assert!(check_expected_counts(&plan, None, None).is_ok());
        assert!(check_expected_counts(&plan, Some(2), Some(1)).is_ok());
        let err = check_expected_counts(&plan, Some(3), Some(1)).unwrap_err();
        assert!(matches!(err, AppError::Other(m) if m.contains("changed since the preview")));
        assert!(check_expected_counts(&plan, Some(2), Some(0)).is_err());
    }

    #[test]
    fn preview_on_fixture_counts_all_messages() {
        let (_tmp, db_path, _att) = fresh_fixture();
        let conn = writable(&db_path);
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

        let mut conn = writable(&db_path);
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
    fn deleting_messages_takes_their_attachment_records_with_them() {
        // Why "messages only" was removed: with chat.db's real triggers,
        // removing a message's last link deletes the attachment record.
        let (_tmp, db_path, _att) = fresh_fixture();
        let mut conn = writable(&db_path);
        let plan = DeletePlan {
            message_rowids: vec![1, 2],
            ..Default::default()
        };
        execute_delete(&mut conn, &plan).unwrap();
        let n_att: i64 = conn
            .query_row("SELECT COUNT(*) FROM attachment", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_att, 0);
        // …and every deletion is queued for iCloud sync.
        let synced: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_deleted_messages", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(synced, 2);
    }

    #[test]
    fn snapshot_of_fixture_is_independent_of_subsequent_delete() {
        let (tmp, db_path, _att) = fresh_fixture();
        let snap_root = tmp.path().join("snaps");

        let snap = crate::core::snapshot::snapshot_chat_db(&db_path, &snap_root).unwrap();

        // Execute the delete on the source db.
        let mut conn = writable(&db_path);
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
