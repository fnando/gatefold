//! One `gatefold run` per state dir: an advisory lock on `gatefold.lock`,
//! holding the daemon's pid.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Held until dropped.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Take the lock at `path` without waiting.
pub fn acquire(path: &Path) -> Result<Lock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Don't truncate before locking: the holder's pid must stay readable.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            let pid = text.trim();
            bail!(
                "another gatefold run is going{}",
                if pid.is_empty() {
                    String::new()
                } else {
                    format!(" (pid {pid})")
                }
            );
        }
        Err(TryLockError::Error(err)) => {
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(Lock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder() {
        let dir = std::env::temp_dir().join(format!("gatefold-lock-{}", std::process::id()));
        let path = dir.join("gatefold.lock");
        let held = acquire(&path).unwrap();
        let err = acquire(&path).unwrap_err().to_string();
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        drop(held);
        assert!(acquire(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
