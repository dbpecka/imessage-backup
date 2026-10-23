//! End-to-end tests: the real backup → coverage → delete → orphan-cleanup
//! flow, run against a real `chat.db` schema (Apple's tables, triggers and
//! cascading foreign keys, in `tests/fixtures/chat-db-schema.sql`) inside a
//! throwaway home folder.
//!
//! Each test gets its own home via [`set_test_home`], which is per-thread,
//! so tests run in parallel and never touch the real `~/Library/Messages`.
//! Attachment paths are stored absolute (the library expands `~` from the
//! real `$HOME`, which the override can't reach); after `~` expansion that
//! is exactly what a real chat.db holds.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use imessage_database::exporters::progress::NoopProgress;
use rusqlite::{params, Connection};
use tempfile::TempDir;

use crate::commands::backup::{backup_now, BackupResult, RunBackupArgs};
use crate::commands::delete::{
    coverage_now, delete_now, preview_delete_now, DeletePreview, DeleteResult, DeleteScope,
    RunDeleteArgs, SafetyProbe, DELETE_CONFIRMATION_PHRASE,
};
use crate::commands::orphans::{clean_orphans_blocking, scan_orphans_blocking, CleanOrphansArgs};
use crate::core::db_path::{open_write, set_test_home};
use crate::core::filter::{DateRange, FilterSpec};
use crate::core::icloud::ICloudState;
use crate::core::receipts::BackupReceipts;
use crate::core::retained::RetainedAttachments;
use crate::error::AppError;

const REAL_SCHEMA: &str = include_str!("../tests/fixtures/chat-db-schema.sql");

/// Seconds between the Unix epoch and Apple's (2001-01-01).
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

/// Noon UTC on the given day, in chat.db's nanoseconds-since-2001.
fn apple_date(y: i32, m: u32, d: u32) -> i64 {
    let unix = chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    (unix - APPLE_EPOCH_OFFSET) * 1_000_000_000
}

fn quiet_mac() -> SafetyProbe {
    SafetyProbe {
        messages_running: || false,
        icloud: || ICloudState::Disabled,
    }
}

/// One throwaway home with a seeded Messages folder.
struct Env {
    _tmp: TempDir,
    home: PathBuf,
    db: PathBuf,
    attachments: PathBuf,
    receipts: BackupReceipts,
    retained: RetainedAttachments,
}

impl Drop for Env {
    fn drop(&mut self) {
        set_test_home(None);
    }
}

impl Env {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("home");
        let messages = home.join("Library/Messages");
        let attachments = messages.join("Attachments");
        fs::create_dir_all(&attachments).unwrap();
        let db = messages.join("chat.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch(REAL_SCHEMA)
            .unwrap();
        set_test_home(Some(home.clone()));

        let env = Env {
            retained: RetainedAttachments::in_dir(&home.join("app-data")),
            receipts: BackupReceipts::default(),
            _tmp: tmp,
            home,
            db,
            attachments,
        };
        env.seed();
        env
    }

    fn conn(&self) -> Connection {
        open_write(&self.db).unwrap()
    }

    /// Write an attachment file under the Messages folder.
    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.attachments.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }

    fn add_attachment(&self, rowid: i64, path: &Path, mime: &str, msg: Option<i64>) {
        let c = self.conn();
        let len = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
        c.execute(
            "INSERT INTO attachment (ROWID, guid, original_guid, filename, mime_type, \
             total_bytes, transfer_name) VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)",
            params![
                rowid,
                format!("att-{rowid}"),
                path.to_str().unwrap(),
                mime,
                len,
                path.file_name().unwrap().to_str().unwrap(),
            ],
        )
        .unwrap();
        if let Some(m) = msg {
            c.execute(
                "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (?1, ?2)",
                params![m, rowid],
            )
            .unwrap();
        }
    }

    fn add_message(&self, rowid: i64, chat: i64, handle: i64, date: i64, text: &str) {
        let c = self.conn();
        c.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id, date, service, is_from_me) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'iMessage', 0)",
            params![rowid, format!("g{rowid}"), text, handle, date],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_message_join (chat_id, message_id, message_date) VALUES (?1, ?2, ?3)",
            params![chat, rowid, date],
        )
        .unwrap();
    }

    /// Two chats:
    /// - chat 1 (+15550001): m1 (2020), m2 (2020, two voice memos both named
    ///   `Audio Message.caf`), m3 (2023, a photo). Attachment 5 is shared
    ///   by m1 and m3.
    /// - chat 2 (friend@example.com): m4 (2020, a photo), m5 (2023, a
    ///   tapback on m1).
    fn seed(&self) {
        let c = self.conn();
        c.execute_batch(
            "INSERT INTO handle (ROWID, id, service) VALUES
               (1, '+15550001', 'iMessage'), (2, 'friend@example.com', 'iMessage');
             INSERT INTO chat (ROWID, guid, chat_identifier, service_name) VALUES
               (1, 'iMessage;-;+15550001', '+15550001', 'iMessage'),
               (2, 'iMessage;-;friend@example.com', 'friend@example.com', 'iMessage');
             INSERT INTO chat_handle_join VALUES (1, 1), (2, 2);",
        )
        .unwrap();
        drop(c);

        let y2020 = apple_date(2020, 6, 1);
        let y2023 = apple_date(2023, 6, 1);
        // Like real messages, each attachment has a U+FFFC placeholder in
        // the text; the HTML/TXT exporter only handles attachments it finds
        // that way.
        self.add_message(1, 1, 1, y2020, "old one \u{FFFC}");
        self.add_message(2, 1, 1, y2020, "\u{FFFC}\u{FFFC}");
        self.add_message(3, 1, 1, y2023, "\u{FFFC}\u{FFFC}");
        self.add_message(4, 2, 2, y2020, "\u{FFFC}");

        let memo1 = self.file("a1/01/GUID-1/Audio Message.caf", b"first memo");
        let memo2 = self.file("a2/02/GUID-2/Audio Message.caf", b"second memo!");
        let recent = self.file("a3/03/GUID-3/IMG_0001.HEIC", b"recent heic bytes");
        let other = self.file("a4/04/GUID-4/IMG_0001.HEIC", b"other chat heic");
        let shared = self.file("a5/05/GUID-5/shared.jpg", b"shared jpeg");
        self.add_attachment(1, &memo1, "audio/x-caf", Some(2));
        self.add_attachment(2, &memo2, "audio/x-caf", Some(2));
        self.add_attachment(3, &recent, "image/heic", Some(3));
        self.add_attachment(4, &other, "image/heic", Some(4));
        self.add_attachment(5, &shared, "image/jpeg", Some(1));
        self.conn()
            .execute(
                "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (3, 5)",
                [],
            )
            .unwrap();

        // m5: a tapback (in chat 2, in 2023) on m1.
        let c = self.conn();
        c.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id, date, associated_message_guid, \
             associated_message_type) VALUES (5, 'g5', 'Loved “old one”', 2, ?1, 'p:0/g1', 2000)",
            params![y2023],
        )
        .unwrap();
        c.execute(
            "INSERT INTO chat_message_join (chat_id, message_id, message_date) VALUES (2, 5, ?1)",
            params![y2023],
        )
        .unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn backup(&self, filter: FilterSpec, format: &str) -> BackupResult {
        backup_now(
            &self.db,
            &self.receipts,
            RunBackupArgs {
                filter,
                format: format.into(),
                destination: self.home.join("Backups").display().to_string(),
                copy_attachments: true,
            },
            &NoopProgress,
        )
        .unwrap()
    }

    fn preview(&self, filter: &FilterSpec) -> DeletePreview {
        preview_delete_now(&self.db, filter).unwrap()
    }

    fn delete(
        &self,
        filter: FilterSpec,
        scope: DeleteScope,
        skip_backup: bool,
    ) -> Result<DeleteResult, AppError> {
        let p = self.preview(&filter);
        let deletes_messages = matches!(scope, DeleteScope::Both);
        delete_now(
            &self.db,
            &self.receipts,
            RunDeleteArgs {
                filter,
                confirmation_phrase: DELETE_CONFIRMATION_PHRASE.into(),
                acknowledge_skip_backup: skip_backup,
                snapshot_root: None,
                delete_scope: scope,
                acknowledge_icloud_sync: false,
                acknowledge_not_on_mac: false,
                expected_message_count: Some(if deletes_messages { p.message_count } else { 0 }),
                expected_attachment_count: Some(p.attachment_count),
            },
            &self.home.join("snapshots"),
            true,
            &quiet_mac(),
        )
    }
}

fn year_2020() -> FilterSpec {
    FilterSpec {
        date_range: Some(DateRange {
            start: Some("2020-01-01".into()),
            end: Some("2020-12-31".into()),
        }),
        ..Default::default()
    }
}

fn chat(id: i32) -> FilterSpec {
    FilterSpec {
        chat_ids: Some(vec![id]),
        ..Default::default()
    }
}

fn find_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    crate::core::attachment_files::walk_files(dir, &mut out);
    out.sort();
    out
}

#[test]
fn backup_then_delete_a_year_end_to_end() {
    let env = Env::new();
    let filter = year_2020();

    // Preview: m1, m2, m4 plus the 2023 tapback on m1. Attachment 5 is also
    // used by the kept 2023 message, so it stays out of the plan.
    let p = env.preview(&filter);
    assert_eq!(p.message_count, 4);
    assert_eq!(p.reaction_count, 1);
    assert_eq!(p.attachment_count, 3);
    assert_eq!(p.not_on_mac_count, 0);
    assert!(
        !coverage_now(&env.db, &env.receipts, &filter)
            .unwrap()
            .covered
    );

    // Back up as JSON with attachments.
    let b = env.backup(filter.clone(), "json");
    let copy = b.attachment_copy.as_ref().unwrap();
    assert_eq!(copy.failed, 0, "{:?}", copy.failures);
    assert!(b.includes_attachments);
    let export = PathBuf::from(&b.export_path);
    assert!(export.starts_with(env.home.join("Backups")));

    // Both voice memos are in the backup, with their own contents.
    let copies = find_files(&export.join("attachments"));
    let memos: Vec<String> = copies
        .iter()
        .filter(|p| p.to_string_lossy().ends_with("Audio Message.caf"))
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();
    assert_eq!(memos.len(), 2, "{copies:?}");
    assert!(memos.contains(&"first memo".to_string()));
    assert!(memos.contains(&"second memo!".to_string()));

    // The JSON points at those copies, and the tapback is saved alongside.
    let ndjson: String = fs::read_dir(&export)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "ndjson"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect();
    assert!(ndjson.contains("\"copiedTo\":\"attachments/"), "{ndjson}");
    let tapbacks = fs::read_to_string(export.join("tapbacks-outside-filter.json")).unwrap();
    assert!(tapbacks.contains("p:0/g1"));

    // The backup now covers exactly this delete.
    let cov = coverage_now(&env.db, &env.receipts, &filter).unwrap();
    assert!(cov.covered && cov.with_attachments);

    let r = env
        .delete(filter.clone(), DeleteScope::Both, false)
        .unwrap();
    assert!(r.backup_verified);
    assert_eq!(r.messages_deleted, 4);
    assert_eq!(r.on_disk_files_removed, 3);

    // 2023 message, its photo and the shared attachment survive.
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 1);
    assert_eq!(
        env.count("SELECT COUNT(*) FROM attachment ORDER BY ROWID"),
        2
    );
    assert!(env.attachments.join("a3/03/GUID-3/IMG_0001.HEIC").exists());
    assert!(env.attachments.join("a5/05/GUID-5/shared.jpg").exists());
    assert!(!env
        .attachments
        .join("a1/01/GUID-1/Audio Message.caf")
        .exists());
    assert!(
        !env.attachments.join("a1").exists(),
        "emptied folders removed"
    );
    // Apple's triggers queued the deletions for iCloud.
    assert_eq!(env.count("SELECT COUNT(*) FROM sync_deleted_messages"), 4);

    // The snapshot still has everything.
    let snap = Connection::open(Path::new(&r.snapshot_path).join("chat.db")).unwrap();
    let n: i64 = snap
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 5);

    // Nothing left to delete for that year.
    assert_eq!(env.preview(&filter).message_count, 0);
}

#[test]
fn delete_without_backup_needs_explicit_skip() {
    let env = Env::new();
    let err = env.delete(chat(2), DeleteScope::Both, false).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("No backup covers")));
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 5);

    let r = env.delete(chat(2), DeleteScope::Both, true).unwrap();
    assert!(!r.backup_verified);
    assert_eq!(
        env.count("SELECT COUNT(*) FROM message WHERE ROWID IN (4, 5)"),
        0
    );
}

#[test]
fn message_arriving_after_backup_breaks_coverage() {
    let env = Env::new();
    env.backup(chat(2), "json");
    assert!(
        coverage_now(&env.db, &env.receipts, &chat(2))
            .unwrap()
            .covered
    );

    env.add_message(6, 2, 2, apple_date(2024, 1, 1), "new since the backup");
    assert!(
        !coverage_now(&env.db, &env.receipts, &chat(2))
            .unwrap()
            .covered
    );
    let err = env.delete(chat(2), DeleteScope::Both, false).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("No backup covers")));
    assert_eq!(env.count("SELECT COUNT(*) FROM message WHERE ROWID = 6"), 1);
}

#[test]
fn attachment_downloaded_after_backup_is_not_vouched_for() {
    let env = Env::new();
    // Attachment 6 on m4 is still in iCloud when the backup runs.
    let offloaded = env.attachments.join("a6/06/GUID-6/later.jpg");
    env.add_attachment(6, &offloaded, "image/jpeg", Some(4));
    let b = env.backup(chat(2), "json");
    assert_eq!(b.attachment_copy.as_ref().unwrap().not_on_mac, 1);

    // The user downloads it, then tries to delete.
    env.file("a6/06/GUID-6/later.jpg", b"downloaded later");
    let cov = coverage_now(&env.db, &env.receipts, &chat(2)).unwrap();
    assert!(cov.covered && !cov.with_attachments);
    let err = env.delete(chat(2), DeleteScope::Both, false).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("didn't copy every attachment")));
    assert!(offloaded.exists());
}

#[test]
fn attachments_only_delete_keeps_messages() {
    let env = Env::new();
    let filter = chat(2);
    env.backup(filter.clone(), "json");
    let r = env
        .delete(filter, DeleteScope::AttachmentsOnly, false)
        .unwrap();
    assert_eq!(r.messages_deleted, 0);
    assert_eq!(r.on_disk_files_removed, 1);
    assert_eq!(env.count("SELECT COUNT(*) FROM message WHERE ROWID = 4"), 1);
    assert_eq!(
        env.count("SELECT COUNT(*) FROM attachment WHERE ROWID = 4"),
        0
    );
    assert_eq!(
        env.count("SELECT cache_has_attachments FROM message WHERE ROWID = 4"),
        0
    );
    assert!(!env.attachments.join("a4/04/GUID-4/IMG_0001.HEIC").exists());
}

#[test]
fn messages_only_delete_is_refused() {
    let env = Env::new();
    let err = env
        .delete(chat(2), DeleteScope::MessagesOnly, true)
        .unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("isn't possible")));
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 5);
}

#[test]
fn contact_filter_is_refused_rather_than_deleting_everything() {
    let env = Env::new();
    let filter = FilterSpec {
        handle_ids: Some(vec![2]),
        ..Default::default()
    };
    assert!(preview_delete_now(&env.db, &filter).is_err());
    let err = delete_now(
        &env.db,
        &env.receipts,
        RunDeleteArgs {
            filter,
            confirmation_phrase: DELETE_CONFIRMATION_PHRASE.into(),
            acknowledge_skip_backup: true,
            snapshot_root: None,
            delete_scope: DeleteScope::Both,
            acknowledge_icloud_sync: true,
            acknowledge_not_on_mac: true,
            expected_message_count: None,
            expected_attachment_count: None,
        },
        &env.home.join("snapshots"),
        true,
        &quiet_mac(),
    )
    .unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("contact")));
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 5);
}

#[test]
fn orphan_cleanup_removes_only_true_orphans() {
    let env = Env::new();
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    let age = |p: &Path| {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    };
    // An unlinked attachment record with its file.
    let unlinked = env.file("a7/07/GUID-7/unlinked.jpg", b"unlinked");
    env.add_attachment(7, &unlinked, "image/jpeg", None);
    // An old file nothing refers to.
    let stray = env.file("a8/08/GUID-8/stray.jpg", b"stray");
    age(&stray);
    // A Live Photo video next to a referenced photo.
    let companion = env.file("a3/03/GUID-3/IMG_0001.MOV", b"live photo video");
    age(&companion);
    // A brand-new untracked file (could be an in-flight download).
    let fresh = env.file("a9/09/GUID-9/incoming.jpg", b"incoming");

    let scan = scan_orphans_blocking(&env.retained, false).unwrap();
    assert_eq!(scan.db_orphan_count, 1);
    assert_eq!(scan.fs_orphan_count, 1);

    let r = clean_orphans_blocking(
        &env.retained,
        &env.home.join("orphan-snapshots"),
        &CleanOrphansArgs {
            scan_fingerprint: scan.fingerprint,
            include_retained: false,
        },
        || false,
    )
    .unwrap();
    assert_eq!(r.db_rows_deleted, 1);
    assert_eq!(r.db_files_removed + r.fs_files_removed, 2);
    assert!(r.snapshot_path.is_some());
    assert!(!unlinked.exists() && !stray.exists());
    assert!(
        companion.exists(),
        "companion of a referenced photo is kept"
    );
    assert!(fresh.exists(), "recent files are never orphans");
    assert_eq!(env.count("SELECT COUNT(*) FROM attachment"), 5);
}

#[test]
fn orphan_clean_refuses_if_anything_changed_since_the_scan() {
    let env = Env::new();
    let unlinked = env.file("a7/07/GUID-7/unlinked.jpg", b"unlinked");
    env.add_attachment(7, &unlinked, "image/jpeg", None);
    let scan = scan_orphans_blocking(&env.retained, false).unwrap();

    // It gets linked to a message before the user confirms.
    env.conn()
        .execute(
            "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (3, 7)",
            [],
        )
        .unwrap();
    let err = clean_orphans_blocking(
        &env.retained,
        &env.home.join("orphan-snapshots"),
        &CleanOrphansArgs {
            scan_fingerprint: scan.fingerprint,
            include_retained: false,
        },
        || false,
    )
    .unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("changed since the scan")));
    assert!(unlinked.exists());
}

#[test]
fn html_backup_keeps_original_attachment_bytes() {
    let env = Env::new();
    let b = env.backup(chat(1), "html");
    let copy = b.attachment_copy.as_ref().unwrap();
    assert_eq!(copy.failed, 0, "{:?}", copy.failures);
    assert_eq!(copy.copied, 4);
    // The HEIC photo is copied byte-for-byte, not converted to JPEG.
    let export = PathBuf::from(&b.export_path);
    let heic: Vec<PathBuf> = find_files(&export.join("attachments"))
        .into_iter()
        .filter(|p| p.file_stem().is_some_and(|s| s == "3"))
        .collect();
    assert_eq!(heic.len(), 1, "{heic:?}");
    assert_eq!(fs::read(&heic[0]).unwrap(), b"recent heic bytes");
    assert!(b.includes_attachments);
}
