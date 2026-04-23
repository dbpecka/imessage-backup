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

impl FilterSpec {
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
