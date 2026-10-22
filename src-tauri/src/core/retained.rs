use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Attachment rows the user chose to keep when running a "messages only"
/// delete. Deleting the messages also removes their
/// `message_attachment_join` rows, which makes these attachments look like
/// orphans; the orphan cleaner must skip them or it would delete exactly
/// what the user asked to keep.
pub struct RetainedAttachments {
    path: PathBuf,
}

const FILE_NAME: &str = "retained_attachments.json";

impl RetainedAttachments {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// The store for this app install, under the app data directory.
    pub fn for_app(app: &tauri::AppHandle) -> Result<Self, AppError> {
        use tauri::Manager;
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| AppError::Other(format!("cannot locate app data directory: {e}")))?;
        Ok(Self::in_dir(&dir))
    }

    pub fn load(&self) -> Result<BTreeSet<i64>, AppError> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(e) => return Err(e.into()),
        };
        if let Ok(set) = serde_json::from_slice(&bytes) {
            return Ok(set);
        }
        // Damaged (shouldn't happen with atomic writes, but a failing disk
        // or a hand edit can do it). Refusing to load would block every
        // cleanup and messages-only delete for good; starting empty would
        // silently unprotect kept attachments. Instead keep every number
        // we can still find — protecting a few extra ids is harmless — and
        // set the damaged file aside for inspection.
        let salvaged = salvage_ids(&bytes);
        let aside = self.path.with_extension("json.damaged");
        eprintln!(
            "retained: {} was unreadable; recovered {} ids, original kept at {}",
            self.path.display(),
            salvaged.len(),
            aside.display()
        );
        fs::copy(&self.path, &aside)?;
        self.save(&salvaged)?;
        Ok(salvaged)
    }

    /// Merge `ids` into the persisted set and return the ones that weren't
    /// already in it, so a caller can undo exactly its own addition with
    /// [`Self::remove`].
    pub fn add(&self, ids: &[i64]) -> Result<Vec<i64>, AppError> {
        let mut set = self.load()?;
        let added: Vec<i64> = ids.iter().copied().filter(|id| set.insert(*id)).collect();
        if !added.is_empty() {
            self.save(&set)?;
        }
        Ok(added)
    }

    /// Remove `ids` from the persisted set.
    pub fn remove(&self, ids: &[i64]) -> Result<(), AppError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut set = self.load()?;
        let before = set.len();
        for id in ids {
            set.remove(id);
        }
        if set.len() != before {
            self.save(&set)?;
        }
        Ok(())
    }

    /// Written via a temp file + rename so a crash can't leave a truncated
    /// list behind.
    fn save(&self, set: &BTreeSet<i64>) -> Result<(), AppError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec(set).map_err(|e| AppError::Other(e.to_string()))?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Every integer in `bytes`, however mangled the surrounding JSON is.
fn salvage_ids(bytes: &[u8]) -> BTreeSet<i64> {
    String::from_utf8_lossy(bytes)
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .filter_map(|t| t.parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_and_merges() {
        let tmp = TempDir::new().unwrap();
        let r = RetainedAttachments::in_dir(&tmp.path().join("nested"));
        assert!(r.load().unwrap().is_empty());
        assert_eq!(r.add(&[3, 1]).unwrap(), vec![3, 1]);
        assert_eq!(r.add(&[2, 3]).unwrap(), vec![2]);
        assert_eq!(r.load().unwrap(), BTreeSet::from([1, 2, 3]));

        // Undoing the second add leaves ids that were already there.
        r.remove(&[2]).unwrap();
        assert_eq!(r.load().unwrap(), BTreeSet::from([1, 3]));
    }

    #[test]
    fn damaged_list_is_salvaged_not_dropped() {
        let tmp = TempDir::new().unwrap();
        let r = RetainedAttachments::in_dir(tmp.path());
        fs::write(tmp.path().join(FILE_NAME), b"[12,34,5\x00garbage").unwrap();
        assert_eq!(r.load().unwrap(), BTreeSet::from([5, 12, 34]));
        assert!(tmp
            .path()
            .join("retained_attachments.json.damaged")
            .exists());
        // Rewritten as valid JSON.
        assert_eq!(r.load().unwrap(), BTreeSet::from([5, 12, 34]));
        r.add(&[7]).unwrap();
        assert_eq!(r.load().unwrap(), BTreeSet::from([5, 7, 12, 34]));
    }
}
