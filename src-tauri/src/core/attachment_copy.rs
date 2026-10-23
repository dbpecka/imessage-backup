//! Attachment copying for backups.
//!
//! The library's `copy_single_attachment` names copies after the source
//! file's basename and silently skips (while counting as copied) any name
//! already present — so every voice memo after the first (`Audio
//! Message.caf`) or a second `IMG_0001.HEIC` in a chat was never backed up.
//! It also aborts the whole export on the first unreadable file. This pass
//! names each copy uniquely by attachment rowid and counts per-file outcomes
//! instead.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use imessage_database::{
    exporters::progress::ProgressReporter,
    tables::{attachment::Attachment, messages::message::Message, table::Table},
    util::{platform::Platform, query_context::QueryContext},
};
use rusqlite::Connection;
use serde::Serialize;

use crate::core::attachment_files::walk_files;
use crate::error::AppError;

const ATTACHMENTS_DIR: &str = "attachments";
/// How many individual failures to report back to the UI.
const MAX_REPORTED_FAILURES: usize = 20;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopySummary {
    /// Distinct attachments matched by the filter.
    pub attempted: u64,
    pub copied: u64,
    pub bytes: u64,
    /// The record points at a file that isn't on this Mac (typically
    /// offloaded to iCloud). Nothing could be copied.
    pub not_on_mac: u64,
    /// The record has no file path at all.
    pub unresolvable: u64,
    /// The file is on this Mac but copying it failed.
    pub failed: u64,
    /// The first few failures, as "path: reason".
    pub failures: Vec<String>,
    /// Rowids of the attachments actually copied, for the backup receipt.
    #[serde(skip)]
    pub copied_ids: Vec<i64>,
}

impl CopySummary {
    fn fail(&mut self, what: &Path, why: impl std::fmt::Display) {
        self.failed += 1;
        if self.failures.len() < MAX_REPORTED_FAILURES {
            self.failures.push(format!("{}: {why}", what.display()));
        }
    }
}

/// Visit every distinct attachment of every message matching `query`, with
/// the chat it belongs to and its resolved source path.
fn for_each_attachment(
    conn: &Connection,
    db_path: &Path,
    query: &QueryContext,
    mut f: impl FnMut(Option<i32>, &Attachment, Option<PathBuf>),
) -> Result<(), AppError> {
    let mut seen: HashSet<i32> = HashSet::new();
    let mut stmt = Message::stream_rows(conn, query)?;
    let rows = stmt.query_map([], |row| Ok(Message::from_row(row)))?;
    for row in rows {
        let message = Message::extract(row)?;
        if !message.has_attachments() {
            continue;
        }
        for attachment in Attachment::from_message(conn, &message)? {
            if !seen.insert(attachment.rowid) {
                continue;
            }
            let src = attachment
                .resolved_attachment_path(&Platform::macOS, db_path, None)
                .map(PathBuf::from);
            f(message.chat_id, &attachment, src);
        }
    }
    Ok(())
}

fn total_attachments(conn: &Connection, query: &QueryContext) -> u64 {
    Message::get_attachment_count(conn, query)
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

/// Messages stamps balloon-plugin payloads with a synthetic
/// `.pluginPayloadAttachment` extension; use the MIME type's instead when
/// it's a common one so the copy opens normally.
fn extension_for_mime(mime: Option<&str>) -> Option<&'static str> {
    Some(match mime? {
        "image/jpeg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/x-m4a" | "audio/mp4" => "m4a",
        "audio/mpeg" => "mp3",
        "audio/x-caf" => "caf",
        _ => return None,
    })
}

/// `<rowid>-<original name>`: unique per attachment, still recognisable.
pub fn copy_file_name(attachment: &Attachment, src: &Path) -> String {
    let raw = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_string());
    let mut name: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '/' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let path = Path::new(&name);
    let is_plugin = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pluginPayloadAttachment"));
    if is_plugin {
        if let Some(ext) = extension_for_mime(attachment.mime_type.as_deref()) {
            name = Path::new(&name)
                .with_extension(ext)
                .to_string_lossy()
                .into_owned();
        }
    }
    // Stay well under the 255-byte file name limit.
    while name.len() > 200 {
        name.pop();
    }
    format!("{}-{name}", attachment.rowid)
}

/// Copy every attachment matching `query` into
/// `<export_path>/attachments/<chat_id>/<rowid>-<name>`. Returns the summary
/// and, per attachment rowid, the copy's path relative to `export_path`.
pub fn copy_attachments(
    conn: &Connection,
    db_path: &Path,
    query: &QueryContext,
    export_path: &Path,
    progress: &dyn ProgressReporter,
) -> Result<(CopySummary, HashMap<i32, String>), AppError> {
    progress.start(total_attachments(conn, query));
    progress.set_message("Copying attachments");

    let mut summary = CopySummary::default();
    let mut copied: HashMap<i32, String> = HashMap::new();

    for_each_attachment(conn, db_path, query, |chat_id, attachment, src| {
        summary.attempted += 1;
        progress.set_position(summary.attempted);

        let Some(src) = src else {
            summary.unresolvable += 1;
            return;
        };
        let meta = match fs::metadata(&src) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                summary.not_on_mac += 1;
                return;
            }
            Err(e) => return summary.fail(&src, e),
        };
        if !meta.is_file() {
            return summary.fail(&src, "not a regular file");
        }

        let chat_dir = chat_id.map_or_else(|| "_orphaned".to_string(), |id| id.to_string());
        let rel = format!(
            "{ATTACHMENTS_DIR}/{chat_dir}/{}",
            copy_file_name(attachment, &src)
        );
        let dest = export_path.join(&rel);
        if let Some(dir) = dest.parent() {
            if let Err(e) = fs::create_dir_all(dir) {
                return summary.fail(&src, e);
            }
        }
        match fs::copy(&src, &dest) {
            Ok(n) if n == meta.len() => {
                // Keep the original date so the archive sorts by when the
                // attachment was received, not when it was backed up.
                if let Ok(modified) = meta.modified() {
                    let set = fs::File::options()
                        .write(true)
                        .open(&dest)
                        .and_then(|f| f.set_modified(modified));
                    if let Err(e) = set {
                        eprintln!("backup: couldn't keep date on {}: {e}", dest.display());
                    }
                }
                summary.copied += 1;
                summary.bytes += n;
                summary.copied_ids.push(i64::from(attachment.rowid));
                copied.insert(attachment.rowid, rel);
            }
            Ok(n) => {
                let _ = fs::remove_file(&dest);
                summary.fail(&src, format!("short copy ({n} of {} bytes)", meta.len()));
            }
            Err(e) => {
                let _ = fs::remove_file(&dest);
                summary.fail(&src, e);
            }
        }
    })?;

    progress.finish();
    Ok((summary, copied))
}

/// Sizes of what the runtime wrote, keyed by file stem (the rowid):
/// `attachments/<chat>/<rowid>[.<ext>]` — a file, or a folder for
/// bundle-type attachments.
fn runtime_copy_sizes(export_path: &Path) -> HashMap<String, u64> {
    let mut by_stem: HashMap<String, u64> = HashMap::new();
    if let Ok(chats) = fs::read_dir(export_path.join(ATTACHMENTS_DIR)) {
        for chat in chats.flatten() {
            if !chat.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(entries) = fs::read_dir(chat.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let len = match entry.file_type() {
                    Ok(t) if t.is_file() => fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    Ok(t) if t.is_dir() => {
                        let mut files = Vec::new();
                        walk_files(&path, &mut files);
                        files
                            .iter()
                            .map(|f| fs::metadata(f).map(|m| m.len()).unwrap_or(0))
                            .sum()
                    }
                    _ => continue,
                };
                *by_stem.entry(stem.to_string()).or_default() += len;
            }
        }
    }

    by_stem
}

/// The HTML/TXT runtime does its own copying (to
/// `attachments/<chat>/<rowid>.<ext>`, possibly converted) but only logs
/// failures. Work out what actually landed in the export so the result is
/// truthful.
pub fn verify_runtime_copies(
    conn: &Connection,
    db_path: &Path,
    query: &QueryContext,
    export_path: &Path,
) -> Result<CopySummary, AppError> {
    let by_stem = runtime_copy_sizes(export_path);

    let mut summary = CopySummary::default();
    for_each_attachment(conn, db_path, query, |_, attachment, src| {
        summary.attempted += 1;
        let Some(src) = src else {
            summary.unresolvable += 1;
            return;
        };
        let Some(expected) = source_size(&src) else {
            summary.not_on_mac += 1;
            return;
        };
        match by_stem.get(&attachment.rowid.to_string()) {
            // The runtime copies byte-for-byte (`Clone` mode), so a size
            // difference means a copy that failed partway; the library only
            // logs those and leaves the partial file behind.
            Some(&bytes) if bytes == expected => {
                summary.copied += 1;
                summary.bytes += bytes;
                summary.copied_ids.push(i64::from(attachment.rowid));
            }
            Some(&bytes) => summary.fail(
                &src,
                format!("incomplete copy ({bytes} of {expected} bytes)"),
            ),
            None => summary.fail(&src, "not found in the export"),
        }
    })?;
    Ok(summary)
}

/// Size of an attachment source: a file's length, or the total of a
/// folder's files. `None` if it isn't on this Mac.
fn source_size(src: &Path) -> Option<u64> {
    let meta = fs::metadata(src).ok()?;
    if meta.is_dir() {
        let mut files = Vec::new();
        walk_files(src, &mut files);
        Some(
            files
                .iter()
                .map(|f| fs::metadata(f).map(|m| m.len()).unwrap_or(0))
                .sum(),
        )
    } else {
        Some(meta.len())
    }
}

/// The JSON export runs in reference mode (so the library doesn't do its
/// own, collision-prone copying); fill each attachment's `copiedTo` in the
/// `.ndjson` files from our copy pass, and record the real counts in
/// `manifest.json`.
pub fn annotate_json_export(
    export_path: &Path,
    manifest_path: &Path,
    copied: &HashMap<i32, String>,
    summary: &CopySummary,
) -> Result<(), AppError> {
    let entries = fs::read_dir(export_path)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("ndjson") {
            rewrite_ndjson(&path, copied)?;
        }
    }

    if let Ok(bytes) = fs::read(manifest_path) {
        if let Ok(mut manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(counts) = manifest.get_mut("counts").and_then(|c| c.as_object_mut()) {
                counts.insert("attachmentBytesCopied".into(), summary.bytes.into());
                counts.insert("attachmentsCopied".into(), summary.copied.into());
                counts.insert("attachmentsNotOnMac".into(), summary.not_on_mac.into());
                counts.insert(
                    "attachmentsUnresolvable".into(),
                    summary.unresolvable.into(),
                );
                counts.insert("attachmentsFailed".into(), summary.failed.into());
            }
            let json =
                serde_json::to_vec_pretty(&manifest).map_err(|e| AppError::Other(e.to_string()))?;
            write_atomic(manifest_path, &json)?;
        }
    }
    Ok(())
}

fn rewrite_ndjson(path: &Path, copied: &HashMap<i32, String>) -> Result<(), AppError> {
    let tmp = path.with_extension("ndjson.tmp");
    {
        let reader = BufReader::new(fs::File::open(path)?);
        let mut out = BufWriter::new(fs::File::create(&tmp)?);
        for line in reader.lines() {
            let line = line?;
            let rewritten = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|mut v| set_copied_to(&mut v, copied).then_some(v));
            match rewritten {
                Some(v) => serde_json::to_writer(&mut out, &v)
                    .map_err(|e| AppError::Other(e.to_string()))?,
                None => out.write_all(line.as_bytes())?,
            }
            out.write_all(b"\n")?;
        }
        out.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Returns whether anything changed.
fn set_copied_to(message: &mut serde_json::Value, copied: &HashMap<i32, String>) -> bool {
    let Some(atts) = message
        .get_mut("attachments")
        .and_then(|a| a.as_array_mut())
    else {
        return false;
    };
    let mut changed = false;
    for att in atts {
        let rel = att
            .get("rowid")
            .and_then(|r| r.as_i64())
            .and_then(|r| i32::try_from(r).ok())
            .and_then(|r| copied.get(&r));
        if let (Some(rel), Some(obj)) = (rel, att.as_object_mut()) {
            obj.insert("copiedTo".into(), rel.clone().into());
            changed = true;
        }
    }
    changed
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn att(rowid: i32, mime: Option<&str>) -> Attachment {
        Attachment {
            rowid,
            filename: None,
            uti: None,
            mime_type: mime.map(str::to_string),
            transfer_name: None,
            total_bytes: 0,
            is_sticker: false,
            hide_attachment: 0,
            emoji_description: None,
            copied_path: None,
        }
    }

    #[test]
    fn copy_names_are_unique_per_attachment() {
        let src = Path::new("/x/Audio Message.caf");
        assert_eq!(copy_file_name(&att(1, None), src), "1-Audio Message.caf");
        assert_eq!(copy_file_name(&att(2, None), src), "2-Audio Message.caf");
    }

    #[test]
    fn plugin_payload_gets_real_extension() {
        let src = Path::new("/x/IMG_1.pluginPayloadAttachment");
        assert_eq!(
            copy_file_name(&att(5, Some("image/heic")), src),
            "5-IMG_1.heic"
        );
        assert_eq!(
            copy_file_name(&att(5, None), src),
            "5-IMG_1.pluginPayloadAttachment"
        );
    }

    #[test]
    fn runtime_copies_are_found_as_files_or_folders() {
        let tmp = TempDir::new().unwrap();
        let chat = tmp.path().join("attachments/7");
        fs::create_dir_all(chat.join("12")).unwrap();
        fs::write(chat.join("12/inner.bin"), b"abcd").unwrap();
        fs::write(chat.join("11.jpeg"), b"xy").unwrap();

        let stems = runtime_copy_sizes(tmp.path());
        assert_eq!(stems.get("11"), Some(&2));
        assert_eq!(stems.get("12"), Some(&4));
    }

    #[test]
    fn source_size_sums_folder_contents() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("a.jpg");
        fs::write(&f, b"abc").unwrap();
        let d = tmp.path().join("bundle");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("x"), b"12").unwrap();
        fs::write(d.join("sub/y"), b"345").unwrap();
        assert_eq!(source_size(&f), Some(3));
        assert_eq!(source_size(&d), Some(5));
        assert_eq!(source_size(&tmp.path().join("missing")), None);
    }

    #[test]
    fn ndjson_copied_to_is_filled_in() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("chat.ndjson");
        fs::write(
            &file,
            "{\"rowid\":1,\"attachments\":[{\"rowid\":7,\"copiedTo\":null},{\"rowid\":8,\"copiedTo\":null}]}\n\
             {\"rowid\":2,\"attachments\":[]}\n",
        )
        .unwrap();
        let manifest = tmp.path().join("manifest.json");
        fs::write(&manifest, r#"{"counts":{"attachments":2}}"#).unwrap();
        let copied = HashMap::from([(7, "attachments/1/7-a.jpg".to_string())]);
        let summary = CopySummary {
            copied: 1,
            not_on_mac: 1,
            bytes: 10,
            ..Default::default()
        };

        annotate_json_export(tmp.path(), &manifest, &copied, &summary).unwrap();

        let lines: Vec<serde_json::Value> = fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0]["attachments"][0]["copiedTo"],
            "attachments/1/7-a.jpg"
        );
        assert!(lines[0]["attachments"][1]["copiedTo"].is_null());
        let m: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(m["counts"]["attachmentsCopied"], 1);
        assert_eq!(m["counts"]["attachmentsNotOnMac"], 1);
    }
}
