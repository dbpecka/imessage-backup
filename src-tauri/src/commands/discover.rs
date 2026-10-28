use crate::core::db_path::{blocking, check_chat_db_access, default_chat_db_path, open_read};
use crate::error::AppError;
use imessage_database::tables::{
    chat::Chat,
    handle::Handle,
    table::{get_connection, Cacheable},
};
use serde::Serialize;
use std::collections::HashMap;

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
    blocking(probe_db_blocking).await
}

fn probe_db_blocking() -> Result<ProbeResult, AppError> {
    let path = default_chat_db_path()?;
    let path_str = path.display().to_string();
    check_chat_db_access(&path)?;

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
    /// Every chat row that makes up this conversation. Messages often keeps
    /// separate rows for the same conversation (iMessage vs SMS/RCS, or
    /// after re-registration); filtering by just one would back up or
    /// delete only part of the history.
    pub chat_ids: Vec<i32>,
}

/// Fold chat rows that share a `chat_identifier` (case-insensitively) into
/// one conversation. The row with the most messages provides the `rowid`
/// and labels; counts, participants and services are combined.
pub(crate) fn merge_chats(rows: Vec<ChatSummary>) -> Vec<ChatSummary> {
    let mut sorted = rows;
    sorted.sort_by_key(|s| std::cmp::Reverse(s.message_count));

    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<ChatSummary> = Vec::new();
    for row in sorted {
        let key = row.chat_identifier.to_lowercase();
        let Some(&i) = by_key.get(&key).filter(|_| !key.is_empty()) else {
            by_key.insert(key, out.len());
            out.push(row);
            continue;
        };
        let merged = &mut out[i];
        merged.chat_ids.extend(row.chat_ids);
        merged.message_count += row.message_count;
        merged.display_name = merged.display_name.take().or(row.display_name);
        merged.contact_name = merged.contact_name.take().or(row.contact_name);
        merged.service_name = match (merged.service_name.take(), row.service_name) {
            (Some(a), Some(b)) if !a.split(", ").any(|s| s == b) => Some(format!("{a}, {b}")),
            (a, b) => a.or(b),
        };
        merged.participant_handles.extend(row.participant_handles);
        merged.participant_handles.sort();
        merged.participant_handles.dedup();
        merged.participant_count = merged
            .participant_count
            .max(row.participant_count)
            .max(merged.participant_handles.len());
    }
    for c in &mut out {
        c.chat_ids.sort_unstable();
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.message_count));
    out
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
    #[cfg(not(target_os = "macos"))]
    let contact_map = crate::core::contacts::ContactMap::default();

    blocking(move || list_chats_blocking(&contact_map)).await
}

fn list_chats_blocking(
    contact_map: &crate::core::contacts::ContactMap,
) -> Result<Vec<ChatSummary>, AppError> {
    let path = default_chat_db_path()?;
    let conn = open_read(&path)?;

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

    let summaries: Vec<ChatSummary> = chats
        .into_iter()
        .map(|(rowid, c)| {
            let contact_name = contact_map.lookup(&c.chat_identifier);

            ChatSummary {
                rowid,
                chat_identifier: c.chat_identifier,
                display_name: c.display_name,
                contact_name,
                service_name: c.service_name,
                participant_count: participant_counts.get(&rowid).copied().unwrap_or(0),
                participant_handles: participant_handles.remove(&rowid).unwrap_or_default(),
                message_count: message_counts.get(&rowid).copied().unwrap_or(0),
                chat_ids: vec![rowid],
            }
        })
        .collect();

    Ok(merge_chats(summaries))
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
    blocking(list_contacts_blocking).await
}

fn list_contacts_blocking() -> Result<Vec<ContactSummary>, AppError> {
    let path = default_chat_db_path()?;
    let conn = open_read(&path)?;

    let handles = Handle::cache(&conn)?;
    let mut contacts: Vec<ContactSummary> = handles
        .into_iter()
        .filter(|(rowid, _)| *rowid != 0) // skip the ME placeholder
        .map(|(rowid, id)| ContactSummary { rowid, id })
        .collect();

    contacts.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(contacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(rowid: i32, id: &str, service: &str, count: i64, handles: &[&str]) -> ChatSummary {
        ChatSummary {
            rowid,
            chat_identifier: id.into(),
            display_name: None,
            contact_name: None,
            service_name: Some(service.into()),
            participant_count: handles.len(),
            participant_handles: handles.iter().map(|h| h.to_string()).collect(),
            message_count: count,
            chat_ids: vec![rowid],
        }
    }

    #[test]
    fn merges_rows_for_the_same_conversation() {
        let merged = merge_chats(vec![
            chat(1, "+15551234567", "SMS", 10, &["+15551234567"]),
            chat(
                2,
                "other@example.com",
                "iMessage",
                5,
                &["other@example.com"],
            ),
            chat(3, "+15551234567", "iMessage", 40, &["+15551234567"]),
        ]);
        assert_eq!(merged.len(), 2);
        let first = &merged[0];
        assert_eq!(first.rowid, 3, "row with most messages is the primary");
        assert_eq!(first.chat_ids, vec![1, 3]);
        assert_eq!(first.message_count, 50);
        assert_eq!(first.service_name.as_deref(), Some("iMessage, SMS"));
        assert_eq!(first.participant_handles, vec!["+15551234567"]);
        assert_eq!(merged[1].chat_ids, vec![2]);
    }
}
