use std::fs;
use std::path::{Component, Path, PathBuf};

/// Directories Messages stores attachment payloads in. Anything this app
/// unlinks from disk must live under one of these.
const ATTACHMENT_DIRS: &[&str] = &[
    "~/Library/Messages/Attachments",
    "~/Library/Messages/StickerCache",
];

/// Expand a leading `~` to the user's home directory, matching the library's
/// own `gen_macos_attachment` behaviour.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(path.replacen('~', &home, 1));
        }
    }
    PathBuf::from(path)
}

pub fn attachment_roots() -> Vec<PathBuf> {
    ATTACHMENT_DIRS.iter().map(|d| expand_tilde(d)).collect()
}

/// Case-insensitive comparison key for a path. APFS is case-insensitive by
/// default, and chat.db paths don't always match on-disk casing.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// `true` when `path` is lexically inside one of `roots` and contains no
/// `..` components that could walk back out.
pub fn is_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let key = path_key(path);
    roots.iter().any(|r| {
        let root = path_key(r);
        key.len() > root.len() && key.starts_with(&root) && key.as_bytes()[root.len()] == b'/'
    })
}

/// Recursively collect regular files under `dir`. Symlinks are never
/// followed or returned, so a link inside the Messages folders can't steer
/// the walk (or a later delete) outside of it, and link cycles can't recurse
/// forever.
pub fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            walk_files(&entry.path(), out);
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    /// Nothing to do: the file is already gone.
    Missing,
    /// Refused: outside the Messages attachment folders, or not a regular
    /// file (symlink, directory, …).
    Refused,
    Failed,
}

/// Unlink an attachment file, but only if it is a regular file inside one of
/// `roots`. chat.db is the source of these paths; a row pointing somewhere
/// else on disk must never turn into deleting an unrelated file.
pub fn remove_attachment_file(path: &Path, roots: &[PathBuf], context: &str) -> RemoveOutcome {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RemoveOutcome::Missing,
        Err(e) => {
            eprintln!("{context}: cannot stat {}: {e}", path.display());
            return RemoveOutcome::Failed;
        }
    };
    if !meta.file_type().is_file() || !is_within_roots(path, roots) {
        eprintln!(
            "{context}: refusing to remove {} (not a regular file inside the Messages attachment folders)",
            path.display()
        );
        return RemoveOutcome::Refused;
    }
    match fs::remove_file(path) {
        Ok(()) => RemoveOutcome::Removed,
        Err(e) => {
            eprintln!("{context}: failed to remove {}: {e}", path.display());
            RemoveOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn within_roots_rejects_escapes_and_prefix_tricks() {
        let roots = vec![PathBuf::from("/Users/a/Library/Messages/Attachments")];
        assert!(is_within_roots(
            Path::new("/Users/a/Library/Messages/Attachments/ab/01/G/x.jpg"),
            &roots
        ));
        assert!(is_within_roots(
            Path::new("/users/A/library/messages/attachments/ab/x.jpg"),
            &roots
        ));
        assert!(!is_within_roots(
            Path::new("/Users/a/Library/Messages/Attachments/../chat.db"),
            &roots
        ));
        assert!(!is_within_roots(
            Path::new("/Users/a/Library/Messages/AttachmentsEvil/x.jpg"),
            &roots
        ));
        assert!(!is_within_roots(
            Path::new("/Users/a/Library/Messages/Attachments"),
            &roots
        ));
        assert!(!is_within_roots(Path::new("/Users/a/Documents/x"), &roots));
    }

    #[test]
    fn remove_refuses_outside_roots_and_symlinks() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("Attachments");
        fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("keep.txt");
        fs::write(&outside, b"x").unwrap();
        let roots = vec![root.clone()];

        assert_eq!(
            remove_attachment_file(&outside, &roots, "test"),
            RemoveOutcome::Refused
        );
        assert!(outside.exists());

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(
                remove_attachment_file(&link, &roots, "test"),
                RemoveOutcome::Refused
            );
            assert!(outside.exists());
        }

        let inside = root.join("a.jpg");
        fs::write(&inside, b"x").unwrap();
        assert_eq!(
            remove_attachment_file(&inside, &roots, "test"),
            RemoveOutcome::Removed
        );
        assert_eq!(
            remove_attachment_file(&inside, &roots, "test"),
            RemoveOutcome::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_symlinks() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(root.join("sub/a.jpg"), b"x").unwrap();
        fs::write(elsewhere.join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("sub/loop")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("secret.txt"), root.join("filelink")).unwrap();

        let mut out = Vec::new();
        walk_files(&root, &mut out);
        assert_eq!(out, vec![root.join("sub/a.jpg")]);
    }
}
