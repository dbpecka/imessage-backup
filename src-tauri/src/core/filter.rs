use crate::error::AppError;
use imessage_database::util::query_context::QueryContext;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FilterSpec {
    pub date_range: Option<DateRange>,
    pub chat_ids: Option<Vec<i32>>,
    pub handle_ids: Option<Vec<i32>>,
    // Reserved for Task #7 (library AttachmentFilter). Accepted on the wire
    // today so the IPC contract is stable; applied once the library lands.
    #[allow(dead_code)]
    pub attachments: Option<AttachmentFilterSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateRange {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AttachmentFilterSpec {
    pub types: Option<Vec<String>>,
    pub min_bytes: Option<i64>,
    pub max_bytes: Option<i64>,
}

/// Canonical form of a [`FilterSpec`], used to check whether a backup taken
/// earlier in the session covers exactly the records a delete will remove.
/// Empty strings and empty id lists are folded to `None`, matching how
/// `to_query_context` (and the library's `QueryContext`) treat them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterKey {
    start: Option<String>,
    end: Option<String>,
    chat_ids: Option<BTreeSet<i32>>,
    handle_ids: Option<BTreeSet<i32>>,
}

fn non_empty_str(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty_set(ids: Option<&Vec<i32>>) -> Option<BTreeSet<i32>> {
    ids.map(|v| v.iter().copied().collect::<BTreeSet<i32>>())
        .filter(|s| !s.is_empty())
}

impl FilterSpec {
    pub fn key(&self) -> FilterKey {
        let range = self.date_range.as_ref();
        FilterKey {
            start: non_empty_str(range.and_then(|r| r.start.as_deref())),
            end: non_empty_str(range.and_then(|r| r.end.as_deref())),
            chat_ids: non_empty_set(self.chat_ids.as_ref()),
            handle_ids: non_empty_set(self.handle_ids.as_ref()),
        }
    }

    pub fn to_query_context(&self) -> Result<QueryContext, AppError> {
        let mut ctx = QueryContext::default();

        if let Some(range) = &self.date_range {
            if let Some(s) = range.start.as_deref().filter(|v| !v.is_empty()) {
                ctx.set_start(s)
                    .map_err(|e| AppError::Other(format!("invalid start date: {e}")))?;
            }
            if let Some(e) = range.end.as_deref().filter(|v| !v.is_empty()) {
                ctx.set_end(e)
                    .map_err(|e| AppError::Other(format!("invalid end date: {e}")))?;
            }
        }

        if let Some(ids) = &self.chat_ids {
            let set: BTreeSet<i32> = ids.iter().copied().collect();
            ctx.set_selected_chat_ids(set);
        }

        if let Some(ids) = &self.handle_ids {
            let set: BTreeSet<i32> = ids.iter().copied().collect();
            ctx.set_selected_handle_ids(set);
        }

        Ok(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(start: Option<&str>, chats: Option<Vec<i32>>) -> FilterSpec {
        FilterSpec {
            date_range: Some(DateRange {
                start: start.map(str::to_string),
                end: None,
            }),
            chat_ids: chats,
            ..Default::default()
        }
    }

    #[test]
    fn key_ignores_order_duplicates_and_empties() {
        assert_eq!(
            spec(Some("2020"), Some(vec![3, 1, 3])).key(),
            spec(Some("2020"), Some(vec![1, 3])).key()
        );
        assert_eq!(
            spec(Some(""), Some(vec![])).key(),
            FilterSpec::default().key()
        );
    }

    #[test]
    fn key_distinguishes_different_filters() {
        assert_ne!(
            spec(Some("2020"), None).key(),
            spec(Some("2021"), None).key()
        );
        assert_ne!(spec(None, Some(vec![1])).key(), spec(None, None).key());
    }
}
