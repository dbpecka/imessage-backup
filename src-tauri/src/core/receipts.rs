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

/// Identifies exactly which messages and attachments a filter matched at a
/// point in time. A filter alone isn't enough: with no end date (or a
/// chats-only filter) the same filter matches new messages that arrived
/// after the backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentFingerprint {
    pub messages: u64,
    pub attachments: u64,
    pub hash: u64,
}

impl ContentFingerprint {
    pub fn of(message_rowids: &[i64], attachment_rowids: &[i64]) -> Self {
        // FNV-1a: stable across runs and Rust versions, unlike
        // `DefaultHasher`, which matters because receipts are persisted.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for ids in [message_rowids, attachment_rowids] {
            let mut sorted = ids.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            for id in sorted {
                feed(&id.to_le_bytes());
            }
            feed(b"|");
        }
        Self {
            messages: message_rowids.len() as u64,
            attachments: attachment_rowids.len() as u64,
            hash,
        }
    }
}

/// Whether completed backups cover a given filter's current content.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    /// A backup of exactly these messages exists and its folder is still there.
    pub covered: bool,
    /// …and it copied every attachment file.
    pub with_attachments: bool,
    /// The covering backup's folder (the one with attachments, if any).
    pub export_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    filter: FilterKey,
    /// What the filter matched when the backup started.
    content: ContentFingerprint,
    /// The attachments whose files were actually copied. Files that
    /// weren't on the Mac then (and were downloaded later) aren't in it.
    copied: ContentFingerprint,
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

    pub fn record(
        &self,
        filter: FilterKey,
        content: ContentFingerprint,
        copied: ContentFingerprint,
        includes_attachments: bool,
        export_path: &Path,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.push(Receipt {
            filter,
            content,
            copied,
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

    /// Whether a successful backup with this filter, of exactly this
    /// content, still exists on disk — and whether it holds a copy of every
    /// attachment file that is on this Mac now (`present`).
    pub fn coverage(
        &self,
        filter: &FilterKey,
        content: &ContentFingerprint,
        present: &ContentFingerprint,
    ) -> Coverage {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let matching = || {
            g.iter()
                .rev()
                .filter(|r| &r.filter == filter && &r.content == content && r.export_path.is_dir())
        };
        let has_files = |r: &Receipt| r.includes_attachments && &r.copied == present;
        let best = matching()
            .find(|r| has_files(r))
            .or_else(|| matching().next());
        match best {
            Some(r) => Coverage {
                covered: true,
                with_attachments: has_files(r),
                export_path: Some(r.export_path.display().to_string()),
            },
            None => Coverage::default(),
        }
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

    fn files(ids: &[i64]) -> ContentFingerprint {
        ContentFingerprint::of(&[], ids)
    }

    #[test]
    fn coverage_requires_matching_filter_content_and_attachments() {
        let tmp = TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let key = FilterSpec::default().key();
        let content = ContentFingerprint::of(&[1, 2], &[10]);
        let present = files(&[10]);
        assert!(!receipts.coverage(&key, &content, &present).covered);

        receipts.record(key.clone(), content, files(&[]), false, tmp.path());
        let c = receipts.coverage(&key, &content, &present);
        assert!(c.covered && !c.with_attachments);

        receipts.record(key.clone(), content, files(&[10]), true, tmp.path());
        assert!(receipts.coverage(&key, &content, &present).with_attachments);
    }

    #[test]
    fn files_downloaded_after_the_backup_are_not_covered() {
        let tmp = TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let key = FilterSpec::default().key();
        let content = ContentFingerprint::of(&[1], &[10, 11]);
        // At backup time only attachment 10 was on the Mac (11 was in iCloud).
        receipts.record(key.clone(), content, files(&[10]), true, tmp.path());
        assert!(
            receipts
                .coverage(&key, &content, &files(&[10]))
                .with_attachments
        );

        // The user downloads 11; the backup doesn't have it.
        let c = receipts.coverage(&key, &content, &files(&[10, 11]));
        assert!(c.covered && !c.with_attachments);
    }

    #[test]
    fn new_messages_since_the_backup_break_coverage() {
        let tmp = TempDir::new().unwrap();
        let receipts = BackupReceipts::default();
        let key = FilterSpec::default().key();
        let f = files(&[10]);
        receipts.record(
            key.clone(),
            ContentFingerprint::of(&[1, 2], &[10]),
            f,
            true,
            tmp.path(),
        );
        // Same filter, but a message (or attachment) arrived afterwards.
        assert!(
            !receipts
                .coverage(&key, &ContentFingerprint::of(&[1, 2, 3], &[10]), &f)
                .covered
        );
        assert!(
            !receipts
                .coverage(&key, &ContentFingerprint::of(&[1, 2], &[10, 11]), &f)
                .covered
        );
        // Order doesn't matter.
        assert!(
            receipts
                .coverage(&key, &ContentFingerprint::of(&[2, 1], &[10]), &f)
                .covered
        );
    }

    #[test]
    fn fingerprint_distinguishes_message_and_attachment_ids() {
        assert_ne!(
            ContentFingerprint::of(&[1], &[2]),
            ContentFingerprint::of(&[2], &[1])
        );
    }

    #[test]
    fn receipts_survive_a_restart_but_not_a_deleted_backup() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("receipts.json");
        let export = tmp.path().join("Bubble Wrap 1");
        fs::create_dir_all(&export).unwrap();
        let key = FilterSpec::default().key();
        let content = ContentFingerprint::of(&[1], &[]);
        let f = files(&[]);

        BackupReceipts::persistent(file.clone()).record(key.clone(), content, f, true, &export);
        let reloaded = BackupReceipts::persistent(file.clone());
        assert!(reloaded.coverage(&key, &content, &f).with_attachments);

        fs::remove_dir_all(&export).unwrap();
        assert!(!reloaded.coverage(&key, &content, &f).covered);
    }
}
