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
        available_space: crate::core::space::system_available,
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
            crate::core::space::system_available,
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
            acknowledge_icloud_sync: false,
        },
        &quiet_mac(),
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
            acknowledge_icloud_sync: false,
        },
        &quiet_mac(),
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

// ---- Regression tests from the final review ----

/// Age a file so the 24-hour guard doesn't protect it.
fn make_old(p: &Path) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60))
        .unwrap();
}

fn scan_and_clean(
    env: &Env,
    icloud_ack: bool,
    probe: &SafetyProbe,
) -> Result<crate::commands::orphans::OrphanCleanResult, AppError> {
    let scan = scan_orphans_blocking(&env.retained, false).unwrap();
    clean_orphans_blocking(
        &env.retained,
        &env.home.join("orphan-snapshots"),
        &CleanOrphansArgs {
            scan_fingerprint: scan.fingerprint,
            include_retained: false,
            acknowledge_icloud_sync: icloud_ack,
        },
        probe,
    )
}

#[test]
fn pdf_backup_does_not_count_for_delete() {
    let env = Env::new();
    // The PDF writer can only store Latin-1: this would come out as `?`.
    env.add_message(6, 2, 2, apple_date(2020, 7, 1), "Привет 😀 it’s");
    let b = env.backup(chat(2), "pdf");
    assert!(!b.counts_for_delete);
    assert!(
        !coverage_now(&env.db, &env.receipts, &chat(2))
            .unwrap()
            .covered
    );
    let err = env.delete(chat(2), DeleteScope::Both, false).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("No backup covers")));

    // A JSON backup of the same thing does count.
    let b = env.backup(chat(2), "json");
    assert!(b.counts_for_delete);
    assert!(
        env.delete(chat(2), DeleteScope::Both, false)
            .unwrap()
            .backup_verified
    );
}

#[test]
fn html_backup_saves_tapbacks_whose_target_is_outside_the_filter() {
    let env = Env::new();
    // m5 (chat 2) is a tapback on m1 (chat 1): the HTML exporter only
    // renders it under m1, which isn't in a chat-2 export.
    let b = env.backup(chat(2), "html");
    let saved =
        fs::read_to_string(Path::new(&b.export_path).join("tapbacks-without-target.json")).unwrap();
    assert!(saved.contains("\"guid\": \"g5\""), "{saved}");
}

#[test]
fn failed_delete_attempts_keep_the_earlier_rollback_snapshot() {
    let env = Env::new();
    let snaps = env.home.join("snapshots");
    let first = env.delete(chat(2), DeleteScope::Both, true).unwrap();
    let rollback = PathBuf::from(&first.snapshot_path);

    // Messages reopens during every later attempt, after the snapshot.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn reopens() -> bool {
        CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 2 == 1
    }
    for _ in 0..(crate::core::snapshot::SNAPSHOTS_TO_KEEP + 2) {
        std::thread::sleep(Duration::from_millis(1100));
        let p = env.preview(&chat(1));
        let r = delete_now(
            &env.db,
            &env.receipts,
            RunDeleteArgs {
                filter: chat(1),
                confirmation_phrase: DELETE_CONFIRMATION_PHRASE.into(),
                acknowledge_skip_backup: true,
                snapshot_root: None,
                delete_scope: DeleteScope::Both,
                acknowledge_icloud_sync: false,
                acknowledge_not_on_mac: false,
                expected_message_count: Some(p.message_count),
                expected_attachment_count: Some(p.attachment_count),
            },
            &snaps,
            true,
            &SafetyProbe {
                messages_running: reopens,
                icloud: || ICloudState::Disabled,
                available_space: crate::core::space::system_available,
            },
        );
        assert!(r.is_err());
    }
    assert!(
        rollback.exists(),
        "failed attempts pruned the real rollback"
    );
    // …and they left no snapshots of their own behind.
    assert_eq!(fs::read_dir(&snaps).unwrap().count(), 1);
}

#[test]
fn attachments_shared_across_chunks_are_counted_once() {
    let env = Env::new();
    // 900 messages, so the library splits them into two chunks of ≤800;
    // attachment 9 is linked from one message in each.
    {
        let c = env.conn();
        for i in 0..900i64 {
            let id = 100 + i;
            c.execute(
                "INSERT INTO message (ROWID, guid, text, handle_id, date, service) \
                 VALUES (?1, ?2, '\u{FFFC}', 2, ?3, 'iMessage')",
                params![id, format!("bulk{id}"), apple_date(2021, 1, 1) + i],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_message_join (chat_id, message_id, message_date) VALUES (2, ?1, 0)",
                params![id],
            )
            .unwrap();
        }
    }
    let f = env.file("b9/09/G/x.jpg", b"jpeg");
    env.add_attachment(9, &f, "image/jpeg", Some(100));
    env.conn()
        .execute(
            "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (999, 9)",
            [],
        )
        .unwrap();
    let p = env.preview(&chat(2));
    // Attachment 4 (m4) and attachment 9, each once.
    assert_eq!(p.attachment_count, 2);
    assert_eq!(p.attachment_bytes, 15 + 4);
}

#[test]
fn orphan_clean_requires_icloud_acknowledgement() {
    let env = Env::new();
    let f = env.file("a7/07/GUID-7/unlinked.jpg", b"unlinked");
    env.add_attachment(7, &f, "image/jpeg", None);
    let icloud_on = SafetyProbe {
        messages_running: || false,
        icloud: || ICloudState::Enabled,
        available_space: crate::core::space::system_available,
    };
    let err = scan_and_clean(&env, false, &icloud_on).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("iCloud")));
    assert!(f.exists());

    let r = scan_and_clean(&env, true, &icloud_on).unwrap();
    assert_eq!(r.db_rows_deleted, 1);
}

#[test]
fn recently_created_unlinked_record_is_not_an_orphan() {
    let env = Env::new();
    let f = env.file("a7/07/GUID-7/just-arrived.jpg", b"new");
    env.add_attachment(7, &f, "image/jpeg", None);
    // Created a minute ago — its message may not have synced yet.
    let now_apple = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - APPLE_EPOCH_OFFSET
        - 60;
    env.conn()
        .execute(
            "UPDATE attachment SET created_date = ?1 WHERE ROWID = 7",
            params![now_apple],
        )
        .unwrap();
    let scan = scan_orphans_blocking(&env.retained, false).unwrap();
    assert_eq!(scan.db_orphan_count, 0);
    assert!(f.exists());
}

#[cfg(unix)]
#[test]
fn linked_file_stored_via_symlinked_home_is_kept() {
    let env = Env::new();
    let alias = env.home.parent().unwrap().join("old-home-name");
    std::os::unix::fs::symlink(&env.home, &alias).unwrap();
    let real = env
        .home
        .join("Library/Messages/StickerCache/8e-GUID/GUID.heic");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, b"sticker").unwrap();
    make_old(&real);
    let stored = alias.join("Library/Messages/StickerCache/8e-GUID/GUID.heic");
    env.add_attachment(20, &stored, "image/heic", Some(3));

    let r = scan_and_clean(&env, false, &quiet_mac()).unwrap();
    assert_eq!(r.fs_files_removed, 0);
    assert!(real.exists());
}

#[test]
fn linked_file_stored_under_a_renamed_home_is_kept() {
    let env = Env::new();
    // chat.db still says /Users/<old name>/…, which no longer exists.
    let real = env.file("c1/01/GUID-C/photo.jpg", b"photo");
    make_old(&real);
    let stored = PathBuf::from(
        "/Users/old-name-that-does-not-exist/Library/Messages/Attachments/c1/01/GUID-C/photo.jpg",
    );
    env.add_attachment(23, &stored, "image/jpeg", Some(3));
    let r = scan_and_clean(&env, false, &quiet_mac()).unwrap();
    assert_eq!(r.fs_files_removed, 0);
    assert!(real.exists());
}

#[test]
fn linked_file_with_double_slash_path_is_kept() {
    let env = Env::new();
    let f = env.file("b1/01/GUID-B/pic.jpg", b"pic");
    make_old(&f);
    let stored = f.to_str().unwrap().replace("/b1/01/", "/b1//01/");
    env.add_attachment(22, Path::new(&stored), "image/jpeg", Some(3));
    let r = scan_and_clean(&env, false, &quiet_mac()).unwrap();
    assert_eq!(r.fs_files_removed, 0);
    assert!(f.exists());
}

#[test]
fn linked_file_directly_in_a_root_folder_is_kept() {
    let env = Env::new();
    let f = env.home.join("Library/Messages/StickerCache/sticker.heic");
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(&f, b"sticker").unwrap();
    make_old(&f);
    env.add_attachment(21, &f, "image/heic", Some(3));
    let r = scan_and_clean(&env, false, &quiet_mac()).unwrap();
    assert_eq!(r.fs_files_removed, 0);
    assert!(f.exists());
}

#[test]
fn folder_attachment_is_copied_whole_in_json_backups() {
    let env = Env::new();
    // A bundle-style attachment: a folder with nested files (and a symlink,
    // which is skipped rather than followed).
    let bundle = env.attachments.join("d1/01/GUID-D/Pass.pkpass");
    fs::create_dir_all(bundle.join("en.lproj")).unwrap();
    fs::write(bundle.join("pass.json"), b"{\"pass\":1}").unwrap();
    fs::write(bundle.join("en.lproj/strings"), b"hello").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", bundle.join("link")).unwrap();
    env.add_attachment(30, &bundle, "application/vnd.apple.pkpass", Some(4));

    let b = env.backup(chat(2), "json");
    let copy = b.attachment_copy.as_ref().unwrap();
    assert_eq!(copy.failed, 0, "{:?}", copy.failures);
    assert!(b.includes_attachments);

    let export = PathBuf::from(&b.export_path);
    let copied = export.join("attachments/2/30-Pass.pkpass");
    assert_eq!(fs::read(copied.join("pass.json")).unwrap(), b"{\"pass\":1}");
    assert_eq!(fs::read(copied.join("en.lproj/strings")).unwrap(), b"hello");
    assert!(!copied.join("link").exists(), "symlinks are not copied");
    let ndjson: String = fs::read_dir(&export)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "ndjson"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect();
    assert!(ndjson.contains("attachments/2/30-Pass.pkpass"), "{ndjson}");

    // The backup now vouches for the folder, so the delete is verified.
    let cov = coverage_now(&env.db, &env.receipts, &chat(2)).unwrap();
    assert!(cov.with_attachments);
}

#[test]
fn backup_and_delete_refuse_up_front_when_the_disk_is_full() {
    let env = Env::new();
    let almost_full: crate::core::space::SpaceProbe = |_| Ok(1024);
    let err = backup_now(
        &env.db,
        &env.receipts,
        RunBackupArgs {
            filter: chat(1),
            format: "json".into(),
            destination: env.home.join("Backups").display().to_string(),
            copy_attachments: true,
        },
        &NoopProgress,
        almost_full,
    )
    .unwrap_err();
    assert!(
        matches!(&err, AppError::Other(m) if m.contains("Not enough free space")),
        "{err:?}"
    );
    // Nothing half-written was left behind.
    assert!(
        !env.home.join("Backups").exists()
            || fs::read_dir(env.home.join("Backups")).unwrap().count() == 0
    );

    let p = env.preview(&chat(2));
    let err = delete_now(
        &env.db,
        &env.receipts,
        RunDeleteArgs {
            filter: chat(2),
            confirmation_phrase: DELETE_CONFIRMATION_PHRASE.into(),
            acknowledge_skip_backup: true,
            snapshot_root: None,
            delete_scope: DeleteScope::Both,
            acknowledge_icloud_sync: false,
            acknowledge_not_on_mac: false,
            expected_message_count: Some(p.message_count),
            expected_attachment_count: Some(p.attachment_count),
        },
        &env.home.join("snapshots"),
        true,
        &SafetyProbe {
            available_space: almost_full,
            ..quiet_mac()
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, AppError::Other(m) if m.contains("safety snapshot")),
        "{err:?}"
    );
    assert_eq!(
        env.count("SELECT COUNT(*) FROM message"),
        5,
        "nothing deleted"
    );
}

fn snapshot_roots(env: &Env) -> Vec<(crate::core::snapshot::SnapshotKind, PathBuf)> {
    use crate::core::snapshot::SnapshotKind;
    vec![
        (SnapshotKind::Delete, env.home.join("snapshots")),
        (SnapshotKind::OrphanClean, env.home.join("orphan-snapshots")),
        (SnapshotKind::PreRestore, env.home.join("pre-restore")),
    ]
}

fn restore(
    env: &Env,
    snapshot: &str,
    probe: &SafetyProbe,
) -> Result<crate::commands::restore::RestoreResult, AppError> {
    crate::commands::restore::restore_now(
        &env.db,
        &snapshot_roots(env),
        &crate::commands::restore::RestoreArgs {
            snapshot_path: snapshot.into(),
            acknowledge_icloud_sync: false,
        },
        probe,
    )
}

#[test]
fn restore_brings_back_deleted_messages_and_can_itself_be_undone() {
    let env = Env::new();
    let r = env.delete(chat(2), DeleteScope::Both, true).unwrap();
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 3);

    let listed = crate::core::snapshot::list_snapshots(&snapshot_roots(&env));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, crate::core::snapshot::SnapshotKind::Delete);

    let restored = restore(&env, &r.snapshot_path, &quiet_mac()).unwrap();
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 5);
    assert_eq!(
        env.count("SELECT COUNT(*) FROM attachment WHERE ROWID = 4"),
        1
    );

    // The state just before the restore was kept, and restoring it undoes
    // the restore.
    let pre = Connection::open(Path::new(&restored.pre_restore_snapshot).join("chat.db")).unwrap();
    let n: i64 = pre
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3);
    drop(pre);
    restore(&env, &restored.pre_restore_snapshot, &quiet_mac()).unwrap();
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 3);
}

#[test]
fn restore_refuses_anything_that_isnt_one_of_our_snapshots() {
    let env = Env::new();
    // A folder that looks like a snapshot but lives elsewhere.
    let elsewhere = env.home.join("Downloads/2024-01-01T00-00-00");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::copy(&env.db, elsewhere.join("chat.db")).unwrap();
    let err = restore(&env, elsewhere.to_str().unwrap(), &quiet_mac()).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("isn't a Bubble Wrap snapshot")));
    // The live database itself.
    assert!(restore(
        &env,
        env.db.parent().unwrap().to_str().unwrap(),
        &quiet_mac()
    )
    .is_err());
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 5);
}

#[test]
fn restore_refuses_a_damaged_snapshot() {
    let env = Env::new();
    let r = env.delete(chat(2), DeleteScope::Both, true).unwrap();
    let snap_db = Path::new(&r.snapshot_path).join("chat.db");
    // Corrupt it past the header.
    let mut bytes = fs::read(&snap_db).unwrap();
    for b in bytes.iter_mut().skip(4096).take(8192) {
        *b = 0xAB;
    }
    fs::write(&snap_db, bytes).unwrap();
    let _ = fs::remove_file(Path::new(&r.snapshot_path).join("chat.db-wal"));
    assert!(restore(&env, &r.snapshot_path, &quiet_mac()).is_err());
    assert_eq!(
        env.count("SELECT COUNT(*) FROM message"),
        3,
        "live db untouched"
    );
}

#[test]
fn restore_refuses_while_messages_runs_or_without_icloud_ack() {
    let env = Env::new();
    let r = env.delete(chat(2), DeleteScope::Both, true).unwrap();
    let running = SafetyProbe {
        messages_running: || true,
        ..quiet_mac()
    };
    assert!(restore(&env, &r.snapshot_path, &running).is_err());
    let icloud = SafetyProbe {
        icloud: || ICloudState::Enabled,
        ..quiet_mac()
    };
    let err = restore(&env, &r.snapshot_path, &icloud).unwrap_err();
    assert!(matches!(err, AppError::Other(m) if m.contains("iCloud")));
    assert_eq!(env.count("SELECT COUNT(*) FROM message"), 3);
}
