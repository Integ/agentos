//! Crash-safe file replacement, shared by every store that keeps state on disk.
//!
//! A rename alone is atomic but not durable: until the directory entry reaches
//! the disk, a power loss can bring back the old file, or on some filesystems
//! leave an empty one. For stores that record revocations (callers,
//! preauthorizations, the PIN) that would silently undo a security decision,
//! so every replacement goes through here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `contents`.
///
/// After a crash the file holds either the old or the new contents, never a
/// mix; once this returns `Ok` the new contents survive power loss. The file
/// is readable only by its owner: everything the daemons keep is private to
/// the user. The parent directory must already exist.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "an atomic write needs a file name",
        )
    })?;
    let directory = parent(path);
    // Unique per process and call, and created exclusively, so two writers
    // never share a temporary file and a stale one is never written through.
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = directory.join(temp_name);

    let mut created = false;
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        created = true;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_dir(directory)
    })();
    if created && result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Make the entries of `directory` (created, renamed or removed files)
/// durable.
pub fn sync_dir(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(directory)?.sync_all()
    }
    // Directories cannot be opened as files elsewhere; the rename is the best
    // available there.
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "agentos-durable-{tag}-{}-{}",
                std::process::id(),
                crate::now_ms()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_replacement_leaves_only_the_new_contents_behind() {
        let dir = TempDir::new("replace");
        let path = dir.0.join("state.json");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        // No temporary file is left for a later reader to mistake for state.
        assert_eq!(entries(&dir.0), vec!["state.json".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn stored_state_is_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("mode");
        let path = dir.0.join("pin.json");
        write_atomic(&path, b"{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_failed_replacement_keeps_the_old_file_and_cleans_up() {
        let dir = TempDir::new("fail");
        let path = dir.0.join("state.json");
        write_atomic(&path, b"old").unwrap();
        // Renaming a file over a non-empty directory fails on every platform.
        let blocked = dir.0.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("inside"), b"x").unwrap();
        assert!(write_atomic(&blocked, b"new").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(
            entries(&dir.0),
            vec!["blocked".to_string(), "state.json".to_string()]
        );
    }
}
