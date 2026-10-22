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
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                AppError::Other(format!(
                    "failed to read retained attachment list {}: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(e) => Err(e.into()),
        }
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
}
