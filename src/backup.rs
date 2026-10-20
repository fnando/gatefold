use anyhow::{Context, Result};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Writes versioned, tag-only snapshots under
/// `<state>/backup/<digest>/<unix-seconds>/` so a tag that `--replace` overwrites
/// can be restored. The timestamp directory is created lazily on the first
/// snapshot, so tracks that aren't modified leave no trace.
pub struct Backup<'a> {
    root: &'a Path,
    digest: &'a str,
    original_path: &'a Path,
    dir: RefCell<Option<PathBuf>>,
    files: RefCell<Vec<String>>,
}

impl<'a> Backup<'a> {
    pub fn new(root: &'a Path, digest: &'a str, original_path: &'a Path) -> Self {
        Self {
            root,
            digest,
            original_path,
            dir: RefCell::new(None),
            files: RefCell::new(Vec::new()),
        }
    }

    /// The (lazily created) timestamp directory for this track's snapshot.
    fn dir(&self) -> Result<PathBuf> {
        if let Some(dir) = self.dir.borrow().as_ref() {
            return Ok(dir.clone());
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = self.root.join(self.digest).join(ts.to_string());
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        *self.dir.borrow_mut() = Some(dir.clone());
        Ok(dir)
    }

    /// Save a previous tag value under `name` (e.g. `lyrics.txt`, `cover.jpg`).
    pub fn snapshot(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.dir()?.join(name);
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        self.files.borrow_mut().push(name.to_string());
        Ok(())
    }

    /// Write `meta.json` describing the snapshot, if anything was saved. Call once
    /// after a track is processed.
    pub fn finish(&self) -> Result<()> {
        let files = self.files.borrow();
        if files.is_empty() {
            return Ok(());
        }
        let meta = serde_json::json!({
            "original_path": self.original_path.to_string_lossy(),
            "files": *files,
        });
        let path = self.dir()?.join("meta.json");
        fs::write(&path, serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}
