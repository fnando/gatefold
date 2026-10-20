use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// A cover-art image restored from the cache.
pub struct CachedArt {
    pub mime: String,
    pub ext: String,
    pub data: Vec<u8>,
}

/// Sidecar describing the bytes stored next to it at `art/<digest>.<ext>`.
#[derive(Serialize, Deserialize)]
struct ArtMeta {
    ext: String,
    mime: String,
}

/// On-disk cache rooted at `--state-dir`, holding parallel trees:
/// `metadata/<digest>.json` (extracted tags), `lrc/<digest>.json` (lyrics
/// responses), and `art/<digest>.{json,<ext>}` (cover-art images).
#[allow(clippy::struct_field_names)] // the `_dir` suffix is meaningful here
pub struct State {
    metadata_dir: PathBuf,
    lrc_dir: PathBuf,
    art_dir: PathBuf,
    backup_dir: PathBuf,
}

impl State {
    /// Create the state directory layout, ensuring all sub-trees exist.
    pub fn new(root: &Path) -> Result<Self> {
        let metadata_dir = root.join("metadata");
        let lrc_dir = root.join("lrc");
        let art_dir = root.join("art");
        let backup_dir = root.join("backup");
        for dir in [&metadata_dir, &lrc_dir, &art_dir, &backup_dir] {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(Self {
            metadata_dir,
            lrc_dir,
            art_dir,
            backup_dir,
        })
    }

    /// Root of the backup tree (`<state>/backup`).
    pub fn backup_root(&self) -> &Path {
        &self.backup_dir
    }

    fn metadata_path(&self, digest: &str) -> PathBuf {
        self.metadata_dir.join(format!("{digest}.json"))
    }

    fn lrc_path(&self, digest: &str) -> PathBuf {
        self.lrc_dir.join(format!("{digest}.json"))
    }

    fn art_meta_path(&self, digest: &str) -> PathBuf {
        self.art_dir.join(format!("{digest}.json"))
    }

    fn lrc_miss_path(&self, digest: &str) -> PathBuf {
        self.lrc_dir.join(format!("{digest}.miss"))
    }

    fn art_miss_path(&self, key: &str) -> PathBuf {
        self.art_dir.join(format!("{key}.miss"))
    }

    pub fn load_metadata<T: DeserializeOwned>(&self, digest: &str) -> Result<Option<T>> {
        read_json(&self.metadata_path(digest))
    }

    pub fn save_metadata<T: Serialize>(&self, digest: &str, value: &T) -> Result<()> {
        write_json(&self.metadata_path(digest), value)
    }

    pub fn load_response<T: DeserializeOwned>(&self, digest: &str) -> Result<Option<T>> {
        read_json(&self.lrc_path(digest))
    }

    pub fn save_response<T: Serialize>(&self, digest: &str, value: &T) -> Result<()> {
        write_json(&self.lrc_path(digest), value)?;
        self.clear_lyrics_miss(digest)
    }

    pub fn has_response(&self, digest: &str) -> bool {
        self.lrc_path(digest).is_file()
    }

    /// Record that a lyrics lookup returned no match (persisted negative result).
    pub fn save_lyrics_miss(&self, digest: &str) -> Result<()> {
        touch(&self.lrc_miss_path(digest))
    }

    pub fn has_lyrics_miss(&self, digest: &str) -> bool {
        self.lrc_miss_path(digest).is_file()
    }

    fn clear_lyrics_miss(&self, digest: &str) -> Result<()> {
        remove_if_exists(&self.lrc_miss_path(digest))
    }

    /// True once a lyrics lookup has been recorded either way (hit or miss).
    pub fn has_lyrics_state(&self, digest: &str) -> bool {
        self.has_response(digest) || self.has_lyrics_miss(digest)
    }

    pub fn has_art(&self, digest: &str) -> bool {
        self.art_meta_path(digest).is_file()
    }

    /// Record that a cover lookup returned no match (persisted negative result).
    pub fn save_cover_miss(&self, key: &str) -> Result<()> {
        touch(&self.art_miss_path(key))
    }

    pub fn has_cover_miss(&self, key: &str) -> bool {
        self.art_miss_path(key).is_file()
    }

    fn clear_cover_miss(&self, key: &str) -> Result<()> {
        remove_if_exists(&self.art_miss_path(key))
    }

    /// True once a cover lookup has been recorded either way (hit or miss).
    pub fn has_cover_state(&self, key: &str) -> bool {
        self.has_art(key) || self.has_cover_miss(key)
    }

    /// Persist a fetched cover image: metadata sidecar plus the raw bytes.
    pub fn save_art(&self, digest: &str, ext: &str, mime: &str, data: &[u8]) -> Result<()> {
        let bytes_path = self.art_dir.join(format!("{digest}.{ext}"));
        fs::write(&bytes_path, data)
            .with_context(|| format!("writing {}", bytes_path.display()))?;
        write_json(
            &self.art_meta_path(digest),
            &ArtMeta {
                ext: ext.to_string(),
                mime: mime.to_string(),
            },
        )?;
        self.clear_cover_miss(digest)
    }

    /// Load a previously cached cover image, if present.
    pub fn load_art(&self, digest: &str) -> Result<Option<CachedArt>> {
        let Some(meta): Option<ArtMeta> = read_json(&self.art_meta_path(digest))? else {
            return Ok(None);
        };
        let bytes_path = self.art_dir.join(format!("{digest}.{}", meta.ext));
        let data =
            fs::read(&bytes_path).with_context(|| format!("reading {}", bytes_path.display()))?;
        Ok(Some(CachedArt {
            mime: meta.mime,
            ext: meta.ext,
            data,
        }))
    }
}

/// `sha256` of the path string, hex-encoded; used as the per-file cache stem.
pub fn digest_for(path: &Path) -> String {
    digest_str(&path.to_string_lossy())
}

/// `sha256` of an arbitrary string, hex-encoded; used e.g. to key the art cache
/// by `MusicBrainz` release id so it's shared across a release's tracks.
pub fn digest_str(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// Create an empty marker file (used for persisted "not found" results).
fn touch(path: &Path) -> Result<()> {
    fs::write(path, []).with_context(|| format!("writing {}", path.display()))
}

/// Remove a file, treating "already gone" as success.
fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}
