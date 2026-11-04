use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An image restored from the cache.
pub struct CachedArt {
    pub mime: String,
    pub ext: String,
    pub data: Vec<u8>,
    /// Where it came from (e.g. `apple music`), when recorded.
    pub source: Option<String>,
}

/// Sidecar describing the bytes stored next to it at `art/<digest>.<ext>`.
#[derive(Serialize, Deserialize)]
struct ArtMeta {
    ext: String,
    mime: String,
    #[serde(default)]
    source: Option<String>,
}

/// On-disk cache rooted at `--state-dir`, holding parallel trees:
/// `metadata/<digest>.json` (extracted tags), `lrc/<digest>.json` (lyrics
/// responses), `art/<digest>.{json,<ext>}` (downloaded images, cover and artist
/// alike), `artist/<mbid>.{mb.json,wikidata.json,json,miss}` (per-artist link
/// lookups, resolved image URLs, and misses), and `release/<mbid>.{mb.json,json,miss}` (the same for
/// a release's Apple Music artwork, plus `release/group-<mbid>.mb.json` browses).
#[allow(clippy::struct_field_names)] // the `_dir` suffix is meaningful here
pub struct State {
    metadata_dir: PathBuf,
    lrc_dir: PathBuf,
    art_dir: PathBuf,
    artist_dir: PathBuf,
    release_dir: PathBuf,
    backup_dir: PathBuf,
}

impl State {
    /// Create the state directory layout, ensuring all sub-trees exist.
    pub fn new(root: &Path) -> Result<Self> {
        let metadata_dir = root.join("metadata");
        let lrc_dir = root.join("lrc");
        let art_dir = root.join("art");
        let artist_dir = root.join("artist");
        let release_dir = root.join("release");
        let backup_dir = root.join("backup");
        for dir in [
            &metadata_dir,
            &lrc_dir,
            &art_dir,
            &artist_dir,
            &release_dir,
            &backup_dir,
        ] {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(Self {
            metadata_dir,
            lrc_dir,
            art_dir,
            artist_dir,
            release_dir,
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

    fn musicbrainz_artist_path(&self, mbid: &str) -> PathBuf {
        self.artist_dir.join(format!("{mbid}.mb.json"))
    }

    fn wikidata_artist_path(&self, mbid: &str) -> PathBuf {
        self.artist_dir.join(format!("{mbid}.wikidata.json"))
    }

    fn artist_record_path(&self, mbid: &str) -> PathBuf {
        self.artist_dir.join(format!("{mbid}.json"))
    }

    fn artist_miss_path(&self, mbid: &str) -> PathBuf {
        self.artist_dir.join(format!("{mbid}.miss"))
    }

    fn musicbrainz_release_path(&self, mbid: &str) -> PathBuf {
        self.release_dir.join(format!("{mbid}.mb.json"))
    }

    fn musicbrainz_group_path(&self, mbid: &str) -> PathBuf {
        self.release_dir.join(format!("group-{mbid}.mb.json"))
    }

    fn release_art_path(&self, mbid: &str) -> PathBuf {
        self.release_dir.join(format!("{mbid}.json"))
    }

    fn release_miss_path(&self, mbid: &str) -> PathBuf {
        self.release_dir.join(format!("{mbid}.miss"))
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

    /// How long ago the lyrics miss was (last) recorded, if there is one.
    pub fn lyrics_miss_age(&self, digest: &str) -> Option<Duration> {
        age(&self.lrc_miss_path(digest))
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

    /// How long ago the cover miss was (last) recorded, if there is one.
    pub fn cover_miss_age(&self, key: &str) -> Option<Duration> {
        age(&self.art_miss_path(key))
    }

    fn clear_cover_miss(&self, key: &str) -> Result<()> {
        remove_if_exists(&self.art_miss_path(key))
    }

    /// True once a cover lookup has been recorded either way (hit or miss).
    pub fn has_cover_state(&self, key: &str) -> bool {
        self.has_art(key) || self.has_cover_miss(key)
    }

    /// The raw `MusicBrainz` artist lookup (with URL relations), cached per artist.
    pub fn load_musicbrainz_artist(&self, mbid: &str) -> Result<Option<serde_json::Value>> {
        read_json(&self.musicbrainz_artist_path(mbid))
    }

    pub fn save_musicbrainz_artist(&self, mbid: &str, value: &serde_json::Value) -> Result<()> {
        write_json(&self.musicbrainz_artist_path(mbid), value)
    }

    /// The raw Wikidata streaming-links query result (rows or none), cached per artist.
    pub fn load_wikidata_artist(&self, mbid: &str) -> Result<Option<serde_json::Value>> {
        read_json(&self.wikidata_artist_path(mbid))
    }

    pub fn save_wikidata_artist(&self, mbid: &str, value: &serde_json::Value) -> Result<()> {
        write_json(&self.wikidata_artist_path(mbid), value)
    }

    /// The resolved image URLs (and what was last written) for an artist.
    pub fn load_artist_record<T: DeserializeOwned>(&self, mbid: &str) -> Result<Option<T>> {
        read_json(&self.artist_record_path(mbid))
    }

    pub fn save_artist_record<T: Serialize>(&self, mbid: &str, value: &T) -> Result<()> {
        write_json(&self.artist_record_path(mbid), value)?;
        remove_if_exists(&self.artist_miss_path(mbid))
    }

    pub fn has_artist_record(&self, mbid: &str) -> bool {
        self.artist_record_path(mbid).is_file()
    }

    /// Record that an artist has no usable links or no usable image.
    pub fn save_artist_miss(&self, mbid: &str) -> Result<()> {
        touch(&self.artist_miss_path(mbid))
    }

    pub fn has_artist_miss(&self, mbid: &str) -> bool {
        self.artist_miss_path(mbid).is_file()
    }

    /// How long ago the artist miss was (last) recorded, if there is one.
    pub fn artist_miss_age(&self, mbid: &str) -> Option<Duration> {
        age(&self.artist_miss_path(mbid))
    }

    /// True once an artist lookup has been recorded either way (record or miss).
    pub fn has_artist_state(&self, mbid: &str) -> bool {
        self.has_artist_record(mbid) || self.has_artist_miss(mbid)
    }

    /// The raw `MusicBrainz` release lookup (with URL relations), cached per release.
    pub fn load_musicbrainz_release(&self, mbid: &str) -> Result<Option<serde_json::Value>> {
        read_json(&self.musicbrainz_release_path(mbid))
    }

    pub fn save_musicbrainz_release(&self, mbid: &str, value: &serde_json::Value) -> Result<()> {
        write_json(&self.musicbrainz_release_path(mbid), value)
    }

    /// The raw `MusicBrainz` browse of a release group's releases.
    pub fn load_musicbrainz_group(&self, mbid: &str) -> Result<Option<serde_json::Value>> {
        read_json(&self.musicbrainz_group_path(mbid))
    }

    pub fn save_musicbrainz_group(&self, mbid: &str, value: &serde_json::Value) -> Result<()> {
        write_json(&self.musicbrainz_group_path(mbid), value)
    }

    /// Where a release's Apple Music artwork was found.
    pub fn load_release_art<T: DeserializeOwned>(&self, mbid: &str) -> Result<Option<T>> {
        read_json(&self.release_art_path(mbid))
    }

    pub fn save_release_art<T: Serialize>(&self, mbid: &str, value: &T) -> Result<()> {
        write_json(&self.release_art_path(mbid), value)?;
        remove_if_exists(&self.release_miss_path(mbid))
    }

    /// Record that no Apple Music artwork could be found for a release.
    pub fn save_release_miss(&self, mbid: &str) -> Result<()> {
        touch(&self.release_miss_path(mbid))
    }

    /// Persist a fetched image: metadata sidecar plus the raw bytes.
    pub fn save_art(
        &self,
        digest: &str,
        ext: &str,
        mime: &str,
        source: Option<&str>,
        data: &[u8],
    ) -> Result<()> {
        let bytes_path = self.art_dir.join(format!("{digest}.{ext}"));
        fs::write(&bytes_path, data)
            .with_context(|| format!("writing {}", bytes_path.display()))?;
        write_json(
            &self.art_meta_path(digest),
            &ArtMeta {
                ext: ext.to_string(),
                mime: mime.to_string(),
                source: source.map(str::to_string),
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
            source: meta.source,
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
    digest_bytes(value.as_bytes())
}

/// `sha256` of arbitrary bytes, hex-encoded.
pub fn digest_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
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
/// Write (or rewrite) an empty marker, so its mtime is when it was last recorded.
fn touch(path: &Path) -> Result<()> {
    fs::write(path, []).with_context(|| format!("writing {}", path.display()))
}

/// Time since a marker was last written; `None` when it doesn't exist. A marker
/// from the future (clock skew) counts as just written.
fn age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().unwrap_or(Duration::ZERO))
}

/// Remove a file, treating "already gone" as success.
fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_time_since_the_marker_was_written() {
        let dir = std::env::temp_dir().join(format!("gatefold-age-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("x.miss");
        assert_eq!(age(&marker), None);
        touch(&marker).unwrap();
        assert!(age(&marker).unwrap() < Duration::from_mins(1));
        fs::remove_dir_all(&dir).unwrap();
    }
}
