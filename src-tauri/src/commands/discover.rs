use crate::core::db_path::default_chat_db_path;
use crate::error::AppError;
use imessage_database::tables::{
    chat::Chat,
    handle::Handle,
    table::{get_connection, Cacheable},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub path: String,
    pub message_count: i64,
}

/// Probe the live chat.db. Returns the total message count on success.
///
/// Fails with a `FullDiskAccess` error when the file is present but SQLite
/// can't open it — the typical signal that the user hasn't granted FDA yet.
#[tauri::command]
pub async fn probe_db() -> Result<ProbeResult, AppError> {
    let path = default_chat_db_path()?;

    if !path.exists() {
        return Err(AppError::Other(format!(
            "chat.db not found at {}",
            path.display()
        )));
    }

    let path_str = path.display().to_string();

    let conn = match get_connection(&path) {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("unable to open") || msg.contains("authorization denied") {
                return Err(AppError::FullDiskAccess { path: path_str });
            }
            return Err(AppError::Database(e.to_string()));
        }
    };

    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
        .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(ProbeResult {
        path: path_str,
        message_count,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSummary {
    pub rowid: i32,
    pub chat_identifier: String,
    pub display_name: Option<String>,
    pub contact_name: Option<String>,
    pub service_name: Option<String>,
    pub participant_count: usize,
    pub participant_handles: Vec<String>,
    pub message_count: i64,
}

/// List all conversations in chat.db, sorted by message count (descending) so
/// heavy conversations surface first in the picker.
#[tauri::command]
pub async fn list_chats() -> Result<Vec<ChatSummary>, AppError> {
    // Fetch the contacts map concurrently with the db work. On non-macOS the
    // map is empty and contact_name is always None.
    #[cfg(target_os = "macos")]
    let contact_map = tokio::task::spawn_blocking(crate::core::contacts::fetch_contact_map)
        .await
        .map_err(|e| AppError::Other(format!("contact lookup task failed: {e}")))?;

    let path = default_chat_db_path()?;
    let conn = get_connection(&path)?;

    let chats = Chat::cache(&conn)?;

    // Participants per chat via chat_handle_join
    let mut participant_counts: std::collections::HashMap<i32, usize> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT chat_id, COUNT(*) FROM chat_handle_join GROUP BY chat_id")
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok::<(i32, i64), _>((r.get(0)?, r.get(1)?)))
            .map_err(|e| AppError::Database(e.to_string()))?;
        for r in rows {
            let (chat_id, count) = r.map_err(|e| AppError::Database(e.to_string()))?;
            participant_counts.insert(chat_id, count as usize);
        }
    }

    // Participant handle IDs (phone/email) per chat, for labelling group chats
    // whose chat_identifier is an opaque `chatNNN` string.
    let mut participant_handles: std::collections::HashMap<i32, Vec<String>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT chj.chat_id, h.id
                 FROM chat_handle_join chj
                 JOIN handle h ON h.ROWID = chj.handle_id
                 ORDER BY chj.chat_id, h.id",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok::<(i32, String), _>((r.get(0)?, r.get(1)?)))
            .map_err(|e| AppError::Database(e.to_string()))?;
        for r in rows {
            let (chat_id, handle_id) = r.map_err(|e| AppError::Database(e.to_string()))?;
            participant_handles
                .entry(chat_id)
                .or_default()
                .push(handle_id);
        }
    }

    // Message counts per chat via chat_message_join
    let mut message_counts: std::collections::HashMap<i32, i64> = std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT chat_id, COUNT(*) FROM chat_message_join GROUP BY chat_id")
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| Ok::<(i32, i64), _>((r.get(0)?, r.get(1)?)))
            .map_err(|e| AppError::Database(e.to_string()))?;
        for r in rows {
            let (chat_id, count) = r.map_err(|e| AppError::Database(e.to_string()))?;
            message_counts.insert(chat_id, count);
        }
    }

    let mut summaries: Vec<ChatSummary> = chats
        .into_iter()
        .map(|(rowid, c)| {
            #[cfg(target_os = "macos")]
            let contact_name =
                crate::core::contacts::lookup_contact_name(&c.chat_identifier, &contact_map);
            #[cfg(not(target_os = "macos"))]
            let contact_name: Option<String> = None;

            ChatSummary {
                rowid,
                chat_identifier: c.chat_identifier,
                display_name: c.display_name,
                contact_name,
                service_name: c.service_name,
                participant_count: participant_counts.get(&rowid).copied().unwrap_or(0),
                participant_handles: participant_handles.remove(&rowid).unwrap_or_default(),
                message_count: message_counts.get(&rowid).copied().unwrap_or(0),
            }
        })
        .collect();

    summaries.sort_by(|a, b| b.message_count.cmp(&a.message_count));
    Ok(summaries)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactSummary {
    pub rowid: i32,
    pub id: String,
}

/// List unique contacts (phone numbers / emails) from the handle table.
///
/// The library's `Handle::cache` already deduplicates across services via
/// `person_centric_id`, so callers can filter by any rowid that maps to a
/// given contact and the library will resolve the full group.
#[tauri::command]
pub async fn list_contacts() -> Result<Vec<ContactSummary>, AppError> {
    let path = default_chat_db_path()?;
    let conn = get_connection(&path)?;

    let handles = Handle::cache(&conn)?;
    let mut contacts: Vec<ContactSummary> = handles
        .into_iter()
        .filter(|(rowid, _)| *rowid != 0) // skip the ME placeholder
        .map(|(rowid, id)| ContactSummary { rowid, id })
        .collect();

    contacts.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(contacts)
}
