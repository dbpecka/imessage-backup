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

    /// Merge `ids` into the persisted set. Written via a temp file + rename
    /// so a crash can't leave a truncated list behind.
    pub fn add(&self, ids: &[i64]) -> Result<(), AppError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut set = self.load()?;
        set.extend(ids.iter().copied());
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec(&set).map_err(|e| AppError::Other(e.to_string()))?;
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
        r.add(&[3, 1]).unwrap();
        r.add(&[2, 3]).unwrap();
        assert_eq!(r.load().unwrap(), BTreeSet::from([1, 2, 3]));
    }
}
