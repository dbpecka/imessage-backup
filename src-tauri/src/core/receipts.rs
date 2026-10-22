use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::core::filter::FilterKey;

/// How many receipts to keep on disk.
const MAX_RECEIPTS: usize = 100;

/// Completed backups, so `run_delete` can verify a matching backup actually
/// ran instead of trusting a flag sent by the frontend. Persisted to the
/// app data directory so a relaunch (e.g. the forced one after granting
/// Full Disk Access) doesn't force another full backup.
#[derive(Debug, Default)]
pub struct BackupReceipts {
    inner: Mutex<Vec<Receipt>>,
    /// Where receipts are saved; `None` keeps them in memory only.
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    filter: FilterKey,
    includes_attachments: bool,
    /// The run's output folder. A receipt only counts while this still
    /// exists — a deleted backup protects nothing.
    export_path: PathBuf,
}

impl BackupReceipts {
    /// Receipts backed by `path`, loading any saved earlier. An unreadable
    /// file just starts empty: a missing receipt only means the user is
    /// asked to back up again.
    pub fn persistent(path: PathBuf) -> Self {
        let saved = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Receipt>>(&b).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(saved),
            path: Some(path),
        }
    }

    pub fn record(&self, filter: FilterKey, includes_attachments: bool, export_path: &Path) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.push(Receipt {
            filter,
            includes_attachments,
            export_path: export_path.to_path_buf(),
        });
        let excess = g.len().saturating_sub(MAX_RECEIPTS);
        g.drain(..excess);
        if let Some(path) = &self.path {
            if let Err(e) = save(path, &g) {
                eprintln!("receipts: failed to save {}: {e}", path.display());
            }
        }
    }

    /// `true` if a successful backup with exactly this filter ran, its
    /// output still exists, and — when `needs_attachments` — it copied
    /// every attachment file.
    pub fn covers(&self, filter: &FilterKey, needs_attachments: bool) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.iter().any(|r| {
            &r.filter == filter
                && (r.includes_attachments || !needs_attachments)
                && r.export_path.is_dir()
        })
    }
}

fn save(path: &Path, receipts: &[Receipt]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(receipts)?)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::filter::FilterSpec;
    use tempfile::TempDir;

    #[test]
    fn covers_requires_matching_filter_and_attachments() {
        let tmp = TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let key = FilterSpec::default().key();
        assert!(!receipts.covers(&key, false));

        receipts.record(key.clone(), false, tmp.path());
        assert!(receipts.covers(&key, false));
        assert!(!receipts.covers(&key, true));

        receipts.record(key.clone(), true, tmp.path());
        assert!(receipts.covers(&key, true));
    }

    #[test]
    fn receipts_survive_a_restart_but_not_a_deleted_backup() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("receipts.json");
        let export = tmp.path().join("Bubble Wrap 1");
        fs::create_dir_all(&export).unwrap();
        let key = FilterSpec::default().key();

        BackupReceipts::persistent(file.clone()).record(key.clone(), true, &export);
        let reloaded = BackupReceipts::persistent(file.clone());
        assert!(reloaded.covers(&key, true));

        fs::remove_dir_all(&export).unwrap();
        assert!(!reloaded.covers(&key, true));
    }
}
