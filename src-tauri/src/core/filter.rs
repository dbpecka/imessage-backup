use crate::error::AppError;
use imessage_database::util::query_context::QueryContext;
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The library filters with `date <= end`, where `end` is local midnight at
/// the *start* of the given day — so a plain `set_end("2020-12-31")` would
/// leave out everything sent on Dec 31. Users mean "through the end of that
/// day", so set the bound to one nanosecond before the following midnight.
fn set_inclusive_end(ctx: &mut QueryContext, end: &str) -> Result<(), AppError> {
    let invalid = |e: &dyn std::fmt::Display| AppError::Other(format!("invalid end date: {e}"));
    let day =
        chrono::NaiveDate::parse_from_str(end.trim(), "%Y-%m-%d").map_err(|_| invalid(&end))?;
    let next = day.succ_opt().ok_or_else(|| invalid(&end))?;
    ctx.set_end(&next.format("%Y-%m-%d").to_string())
        .map_err(|e| invalid(&e))?;
    ctx.end = ctx.end.map(|ns| ns - 1);
    Ok(())
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
        // The library marks a handle filter as active but its SQL ignores
        // it, so a contact-only filter would quietly match every message —
        // and a delete would target the whole database. Refuse filters
        // that aren't actually applied rather than widen the scope.
        if self.handle_ids.as_ref().is_some_and(|v| !v.is_empty()) {
            return Err(AppError::Other(
                "Filtering by contact isn't supported yet; filter by conversation instead.".into(),
            ));
        }
        if self.attachments.is_some() {
            return Err(AppError::Other(
                "Filtering by attachment type or size isn't supported yet.".into(),
            ));
        }
        let mut ctx = QueryContext::default();

        if let Some(range) = &self.date_range {
            if let Some(s) = range.start.as_deref().filter(|v| !v.is_empty()) {
                ctx.set_start(s)
                    .map_err(|e| AppError::Other(format!("invalid start date: {e}")))?;
            }
            if let Some(e) = range.end.as_deref().filter(|v| !v.is_empty()) {
                set_inclusive_end(&mut ctx, e)?;
            }
        }

        if let Some(ids) = &self.chat_ids {
            let set: BTreeSet<i32> = ids.iter().copied().collect();
            ctx.set_selected_chat_ids(set);
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

    fn ctx_for(start: Option<&str>, end: Option<&str>) -> QueryContext {
        FilterSpec {
            date_range: Some(DateRange {
                start: start.map(str::to_string),
                end: end.map(str::to_string),
            }),
            ..Default::default()
        }
        .to_query_context()
        .unwrap()
    }

    #[test]
    fn end_date_includes_the_whole_end_day() {
        // End of 2020-05-15 is one nanosecond before the start of 2020-05-16.
        let end = ctx_for(None, Some("2020-05-15")).end.unwrap();
        let next_start = ctx_for(Some("2020-05-16"), None).start.unwrap();
        let same_start = ctx_for(Some("2020-05-15"), None).start.unwrap();
        assert_eq!(end, next_start - 1);
        assert!(end > same_start);
    }

    #[test]
    fn end_date_rolls_over_month_and_year() {
        let end = ctx_for(None, Some("2020-12-31")).end.unwrap();
        let next_start = ctx_for(Some("2021-01-01"), None).start.unwrap();
        assert_eq!(end, next_start - 1);
    }

    #[test]
    fn unapplied_filters_are_rejected_not_ignored() {
        let by_contact = FilterSpec {
            handle_ids: Some(vec![3]),
            ..Default::default()
        };
        assert!(by_contact.to_query_context().is_err());
        let empty_contacts = FilterSpec {
            handle_ids: Some(vec![]),
            ..Default::default()
        };
        assert!(!empty_contacts.to_query_context().unwrap().has_filters());
    }

    #[test]
    fn invalid_end_date_is_rejected() {
        let spec = FilterSpec {
            date_range: Some(DateRange {
                start: None,
                end: Some("2020-02-30".into()),
            }),
            ..Default::default()
        };
        assert!(spec.to_query_context().is_err());
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
