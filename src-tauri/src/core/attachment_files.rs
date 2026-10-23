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
        if let Ok(home) = crate::core::db_path::home_dir() {
            return PathBuf::from(path.replacen('~', &home.to_string_lossy(), 1));
        }
    }
    PathBuf::from(path)
}

/// `roots` with symlinks resolved where they exist (e.g. macOS's `/var` →
/// `/private/var`). Everything that compares a resolved path against the
/// roots resolves the roots too, so callers can pass them either way.
pub fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|r| fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
        .collect()
}

/// The attachment folders, with symlinks resolved when they exist, so
/// walked paths and normalised chat.db paths compare like for like.
pub fn attachment_roots() -> Vec<PathBuf> {
    ATTACHMENT_DIRS
        .iter()
        .map(|d| {
            let p = expand_tilde(d);
            fs::canonicalize(&p).unwrap_or(p)
        })
        .collect()
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
    let roots = &canonical_roots(roots);
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RemoveOutcome::Missing,
        Err(e) => {
            eprintln!("{context}: cannot stat {}: {e}", path.display());
            return RemoveOutcome::Failed;
        }
    };
    // Check containment on the real location: resolve symlinks in the
    // parent folders (the file itself must not be a link — checked above via
    // `symlink_metadata`), so a link anywhere up the path can't point the
    // removal outside the Messages folders, and a symlinked home still
    // matches the canonical roots.
    let real = match (path.parent().map(fs::canonicalize), path.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    };
    if !meta.file_type().is_file() || !is_within_roots(&real, roots) {
        eprintln!(
            "{context}: refusing to remove {} (not a regular file inside the Messages attachment folders)",
            path.display()
        );
        return RemoveOutcome::Refused;
    }
    match fs::remove_file(&real) {
        Ok(()) => {
            remove_empty_parents(&real, roots);
            RemoveOutcome::Removed
        }
        Err(e) => {
            eprintln!("{context}: failed to remove {}: {e}", path.display());
            RemoveOutcome::Failed
        }
    }
}

/// After unlinking `path`, remove the folders it leaves empty, walking up
/// to (but never including) the attachment root. `remove_dir` refuses
/// non-empty folders, so this can only ever remove empty ones.
fn remove_empty_parents(path: &Path, roots: &[PathBuf]) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if !is_within_roots(d, roots) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
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
    fn remove_follows_the_real_location_of_parent_folders() {
        let tmp = TempDir::new().unwrap();
        let real_root = tmp.path().join("Attachments");
        fs::create_dir_all(real_root.join("ab")).unwrap();
        let roots = vec![fs::canonicalize(&real_root).unwrap()];
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.jpg"), b"x").unwrap();

        // A folder link inside the root that points outside it.
        std::os::unix::fs::symlink(&outside, real_root.join("ab/escape")).unwrap();
        assert_eq!(
            remove_attachment_file(&real_root.join("ab/escape/keep.jpg"), &roots, "test"),
            RemoveOutcome::Refused
        );
        assert!(outside.join("keep.jpg").exists());

        // The root reached through a linked alias still works.
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real_root, &alias).unwrap();
        fs::write(real_root.join("ab/a.jpg"), b"x").unwrap();
        assert_eq!(
            remove_attachment_file(&alias.join("ab/a.jpg"), &roots, "test"),
            RemoveOutcome::Removed
        );
    }

    #[test]
    fn removing_last_file_prunes_empty_folders_but_not_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("Attachments");
        let roots = vec![root.clone()];
        let a = root.join("ab/01/GUID-1/a.jpg");
        let b1 = root.join("ab/02/GUID-2/b.jpg");
        let b2 = root.join("ab/02/GUID-2/b.mov");
        for f in [&a, &b1, &b2] {
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, b"x").unwrap();
        }

        remove_attachment_file(&a, &roots, "test");
        assert!(!root.join("ab/01").exists(), "emptied folders are removed");
        assert!(root.join("ab").exists(), "non-empty parent stays");

        remove_attachment_file(&b1, &roots, "test");
        assert!(b2.exists() && root.join("ab/02/GUID-2").exists());

        remove_attachment_file(&b2, &roots, "test");
        assert!(root.exists(), "the root itself is never removed");
        assert!(!root.join("ab").exists());
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
