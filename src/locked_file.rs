//! Locked, atomic replacement shared by history and plaintext credentials.
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Replace `path` atomically while holding an exclusive `<path>.<lock>` file.
/// Existing symlinks are followed so managed dotfiles keep their links.
/// `contents` runs under the lock with the resolved path and returns `None`
/// to leave the file untouched. Temporary files start owner-only on Unix.
pub fn replace(
    path: &Path,
    lock: &str,
    contents: impl FnOnce(&Path) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<()> {
    let path = if path.try_exists()? {
        path.canonicalize()?
    } else {
        path.to_owned()
    };
    let parent = path.parent().ok_or_else(|| {
        io::Error::other(format!("{} has no parent directory", path.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension(lock);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    error.kind(),
                    format!(
                        "{} is busy; check for a stale lock at {}",
                        path.display(),
                        lock_path.display()
                    ),
                )
            } else {
                error
            }
        })?;
    let _lock = Lock(lock_path);
    let Some(bytes) = contents(&path)? else {
        return Ok(());
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
