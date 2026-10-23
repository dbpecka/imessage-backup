//! Encrypted backups: an AES-256 encrypted APFS sparse bundle, created and
//! mounted with macOS's own `hdiutil`, so the user can open it later in
//! Finder with just the password — no extra software.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::error::AppError;

/// A backup password. Deserialises from a plain string but never prints:
/// `Debug` is redacted so it can't end up in a log by accident.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Password(String);

impl Password {
    #[cfg(test)]
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

pub const MIN_PASSWORD_LEN: usize = 8;

/// The disk-image operations a backup needs. `hdiutil` in the app; a fake
/// in tests (there's no `hdiutil` off macOS).
pub trait DiskImageTool {
    /// Create an encrypted image at `image` that can grow to `max_bytes`.
    fn create(
        &self,
        image: &Path,
        volume_name: &str,
        password: &Password,
        max_bytes: u64,
    ) -> Result<(), AppError>;
    /// Mount `image` at `mountpoint` (an empty, existing folder).
    fn attach(&self, image: &Path, mountpoint: &Path, password: &Password) -> Result<(), AppError>;
    fn detach(&self, mountpoint: &Path) -> Result<(), AppError>;
}

/// macOS's `hdiutil`. The password is passed on stdin (`-stdinpass`), never
/// as an argument, so it doesn't show up in `ps`.
pub struct Hdiutil;

fn run_hdiutil(args: &[&std::ffi::OsStr], password: Option<&Password>) -> Result<(), AppError> {
    let mut child = Command::new("/usr/bin/hdiutil")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Other(format!("couldn't run hdiutil: {e}")))?;
    if let Some(pw) = password {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Other("couldn't pass the password to hdiutil".into()))?;
        stdin
            .write_all(pw.expose().as_bytes())
            .map_err(|e| AppError::Other(format!("couldn't pass the password to hdiutil: {e}")))?;
        // Dropping stdin closes it, which ends the password.
    }
    let out = child
        .wait_with_output()
        .map_err(|e| AppError::Other(format!("hdiutil failed: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Other(format!(
            "hdiutil {} failed: {}",
            args.first()
                .map(|a| a.to_string_lossy())
                .unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

impl DiskImageTool for Hdiutil {
    fn create(
        &self,
        image: &Path,
        volume_name: &str,
        password: &Password,
        max_bytes: u64,
    ) -> Result<(), AppError> {
        // A sparse bundle only takes the space its contents need; -size is
        // just the most it can grow to.
        let size = format!("{}m", max_bytes.div_ceil(1024 * 1024).max(100));
        let args: Vec<&std::ffi::OsStr> = vec![
            "create".as_ref(),
            "-size".as_ref(),
            size.as_ref(),
            "-type".as_ref(),
            "SPARSEBUNDLE".as_ref(),
            "-fs".as_ref(),
            "APFS".as_ref(),
            "-encryption".as_ref(),
            "AES-256".as_ref(),
            "-stdinpass".as_ref(),
            "-volname".as_ref(),
            volume_name.as_ref(),
            image.as_os_str(),
        ];
        run_hdiutil(&args, Some(password))
    }

    fn attach(&self, image: &Path, mountpoint: &Path, password: &Password) -> Result<(), AppError> {
        let args: Vec<&std::ffi::OsStr> = vec![
            "attach".as_ref(),
            "-stdinpass".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-owners".as_ref(),
            "on".as_ref(),
            "-mountpoint".as_ref(),
            mountpoint.as_os_str(),
            image.as_os_str(),
        ];
        run_hdiutil(&args, Some(password))
    }

    fn detach(&self, mountpoint: &Path) -> Result<(), AppError> {
        let plain: Vec<&std::ffi::OsStr> = vec!["detach".as_ref(), mountpoint.as_os_str()];
        if run_hdiutil(&plain, None).is_ok() {
            return Ok(());
        }
        // Something (Spotlight, Finder) may still hold a file open briefly.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let forced: Vec<&std::ffi::OsStr> =
            vec!["detach".as_ref(), "-force".as_ref(), mountpoint.as_os_str()];
        run_hdiutil(&forced, None)
    }
}

/// A mounted encrypted image. Detaches (and removes the empty mount folder)
/// when dropped, so an export that fails halfway never leaves the backup
/// mounted and readable.
pub struct MountedImage<'a> {
    tool: &'a dyn DiskImageTool,
    pub image: PathBuf,
    pub mountpoint: PathBuf,
    detached: bool,
}

impl<'a> MountedImage<'a> {
    /// Create a new encrypted image next to `destination` and mount it.
    pub fn create_and_mount(
        tool: &'a dyn DiskImageTool,
        destination: &Path,
        name: &str,
        password: &Password,
        max_bytes: u64,
    ) -> Result<Self, AppError> {
        if password.expose().chars().count() < MIN_PASSWORD_LEN {
            return Err(AppError::Other(format!(
                "The backup password must be at least {MIN_PASSWORD_LEN} characters."
            )));
        }
        let image = unique_path(destination, name, "sparsebundle");
        tool.create(&image, name, password, max_bytes)?;
        let mountpoint = std::env::temp_dir().join(format!(
            "bubblewrap-mount-{}-{}",
            std::process::id(),
            image
                .file_stem()
                .map(|s| s.to_string_lossy().replace(' ', "_"))
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&mountpoint)?;
        if let Err(e) = tool.attach(&image, &mountpoint, password) {
            let _ = std::fs::remove_dir(&mountpoint);
            return Err(e);
        }
        Ok(Self {
            tool,
            image,
            mountpoint,
            detached: false,
        })
    }

    /// Unmount now, reporting failure (dropping only logs it).
    pub fn detach(mut self) -> Result<(), AppError> {
        self.detached = true;
        let result = self.tool.detach(&self.mountpoint);
        let _ = std::fs::remove_dir(&self.mountpoint);
        result
    }
}

impl Drop for MountedImage<'_> {
    fn drop(&mut self) {
        if !self.detached {
            if let Err(e) = self.tool.detach(&self.mountpoint) {
                eprintln!(
                    "backup: couldn't unmount {}: {e:?}",
                    self.mountpoint.display()
                );
            }
            let _ = std::fs::remove_dir(&self.mountpoint);
        }
    }
}

/// `dir/name.ext`, or `dir/name-1.ext`, … — a path that doesn't exist yet.
fn unique_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{name}.{ext}"));
    let mut n = 1;
    while p.exists() {
        p = dir.join(format!("{name}-{n}.{ext}"));
        n += 1;
    }
    p
}

/// Test stand-in for `hdiutil`: the "image" is a folder holding the
/// password and a `volume/` folder; "mounting" makes the mountpoint a
/// symlink to that volume, after checking the password.
#[cfg(test)]
pub struct FakeImageTool;

#[cfg(test)]
impl DiskImageTool for FakeImageTool {
    fn create(&self, image: &Path, _: &str, password: &Password, _: u64) -> Result<(), AppError> {
        assert!(!image.exists(), "hdiutil create refuses existing paths");
        std::fs::create_dir_all(image.join("volume"))?;
        std::fs::write(image.join("password"), password.expose())?;
        Ok(())
    }

    fn attach(&self, image: &Path, mountpoint: &Path, password: &Password) -> Result<(), AppError> {
        if std::fs::read_to_string(image.join("password"))? != password.expose() {
            return Err(AppError::Other(
                "hdiutil attach failed: Authentication error".into(),
            ));
        }
        std::fs::remove_dir(mountpoint)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(image.join("volume"), mountpoint)?;
        Ok(())
    }

    fn detach(&self, mountpoint: &Path) -> Result<(), AppError> {
        std::fs::remove_file(mountpoint)?;
        std::fs::create_dir(mountpoint)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_never_prints() {
        let pw = Password::new("hunter2-secret");
        assert!(!format!("{pw:?}").contains("hunter2"));
    }

    #[test]
    fn short_passwords_are_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = MountedImage::create_and_mount(
            &FakeImageTool,
            tmp.path(),
            "b",
            &Password::new("short"),
            1,
        )
        .err()
        .unwrap();
        assert!(matches!(err, AppError::Other(m) if m.contains("at least 8")));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_mounted_image_unmounts_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mountpoint;
        {
            let m = MountedImage::create_and_mount(
                &FakeImageTool,
                tmp.path(),
                "Backup",
                &Password::new("long enough"),
                1,
            )
            .unwrap();
            mountpoint = m.mountpoint.clone();
            std::fs::write(m.mountpoint.join("x"), b"inside").unwrap();
            // An error mid-export: the guard goes out of scope.
        }
        assert!(!mountpoint.exists(), "unmounted and mount folder removed");
        assert_eq!(
            std::fs::read(tmp.path().join("Backup.sparsebundle/volume/x")).unwrap(),
            b"inside"
        );
    }
}
