use std::sync::Mutex;

use crate::core::filter::FilterKey;

/// Backups completed in this app session. Held in Tauri managed state so
/// `run_delete` can verify a matching backup actually ran, instead of
/// trusting a flag sent by the frontend.
#[derive(Debug, Default)]
pub struct BackupReceipts {
    inner: Mutex<Vec<Receipt>>,
}

#[derive(Debug, Clone)]
struct Receipt {
    filter: FilterKey,
    includes_attachments: bool,
}

impl BackupReceipts {
    pub fn record(&self, filter: FilterKey, includes_attachments: bool) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.push(Receipt {
            filter,
            includes_attachments,
        });
    }

    /// `true` if a successful backup with exactly this filter ran, and —
    /// when `needs_attachments` — that backup copied attachment files.
    pub fn covers(&self, filter: &FilterKey, needs_attachments: bool) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.iter()
            .any(|r| &r.filter == filter && (r.includes_attachments || !needs_attachments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::filter::FilterSpec;

    #[test]
    fn covers_requires_matching_filter_and_attachments() {
        let receipts = BackupReceipts::default();
        let key = FilterSpec::default().key();
        assert!(!receipts.covers(&key, false));

        receipts.record(key.clone(), false);
        assert!(receipts.covers(&key, false));
        assert!(!receipts.covers(&key, true));

        receipts.record(key.clone(), true);
        assert!(receipts.covers(&key, true));
    }
}
