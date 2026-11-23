//! An album folder's `.gatefold-archive/`: covers and animated covers
//! replaced by an upload (`covers/`), duplicates `gatefold fix-case` set
//! aside (`case/`), and `.lrc` files replaced by synced ones or an upload
//! (`lyrics/`). [`list`] finds them; [`restore`] brings one back, archiving
//! (and, for a cover, skipping) what it replaces, as `gatefold restore` does
//! for artist files.

use crate::revision::{self, Revisions};
use crate::writer::{self, ARCHIVE_DIR};
use crate::{scanner, skip};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// What an archived file is a version of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Cover,
    CoverVideo,
    /// The `.lrc` of the track with this file stem.
    Lyrics(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub path: PathBuf,
}

/// The archive sections of an album folder, in the order they're listed.
const SECTIONS: &[&str] = &["covers", "case", "lyrics"];

/// A dated archive name's original stem: `cover-2026-11-23-2` → `cover`.
pub fn original_stem(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.rsplitn(5, '-').collect();
    // `<stem>-YYYY-MM-DD` or `<stem>-YYYY-MM-DD-N`.
    for take in [3, 4] {
        if parts.len() > take {
            let original = stem
                .rsplitn(take + 1, '-')
                .last()
                .filter(|s| !s.is_empty())?;
            if revision::is_archived_name(path, original) {
                return Some(original.to_string());
            }
        }
    }
    None
}

/// The kind of version `path` (in section `section`) is.
fn kind_of(section: &str, path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let stem = original_stem(path)?;
    match (section, ext.as_str()) {
        ("lyrics", "lrc") => Some(Kind::Lyrics(stem)),
        ("covers" | "case", "jpg" | "jpeg" | "png") => Some(Kind::Cover),
        ("covers" | "case", "mp4") => Some(Kind::CoverVideo),
        _ => None,
    }
}

/// Every archived version in album folder `dir`, newest first in each section.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    for section in SECTIONS {
        let Ok(files) = fs::read_dir(dir.join(ARCHIVE_DIR).join(section)) else {
            continue;
        };
        let mut found: Vec<Entry> = files
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter_map(|path| {
                Some(Entry {
                    kind: kind_of(section, &path)?,
                    path,
                })
            })
            .collect();
        found.sort_by(|a, b| natord::compare(&b.path.to_string_lossy(), &a.path.to_string_lossy()));
        entries.extend(found);
    }
    entries
}

/// The archived file at `path`, when it's one of album folder `dir`'s.
pub fn entry(dir: &Path, path: &Path) -> Option<Entry> {
    list(dir).into_iter().find(|entry| entry.path == path)
}

/// Bring `entry` back into album folder `dir`. What it replaces is archived
/// (a cover is skipped first, so runs don't write it back). Returns the
/// restored file.
pub fn restore(dir: &Path, entry: &Entry, revisions: &Revisions) -> Result<PathBuf> {
    let ext = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .context("an archived file without an extension")?;
    let (current, target) = match &entry.kind {
        Kind::Cover => (
            writer::existing_cover_sidecar(dir),
            dir.join(format!("cover.{ext}")),
        ),
        Kind::CoverVideo => (
            writer::existing_cover_video(dir),
            dir.join(writer::COVER_VIDEO),
        ),
        Kind::Lyrics(stem) => {
            let track = tracks(dir).into_iter().find(|track| {
                track
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case(stem))
            });
            let target = track.as_ref().map_or_else(
                || dir.join(format!("{stem}.lrc")),
                |track| track.with_extension("lrc"),
            );
            let current = track
                .as_deref()
                .map(scanner::lrc_path_for)
                .filter(|p| p.is_file());
            (current, target)
        }
    };
    if let Some(current) = &current {
        let section = match entry.kind {
            Kind::Lyrics(_) => writer::lyrics_archive_dir(dir),
            Kind::Cover | Kind::CoverVideo => {
                skip::mark(current)?;
                dir.join(ARCHIVE_DIR).join("covers")
            }
        };
        let digest = revision::file_digest(current)?;
        revisions.archive(&section, current, &digest)?;
    }
    if target.exists() {
        bail!("{} is in the way", target.display());
    }
    fs::rename(&entry.path, &target)
        .with_context(|| format!("restoring {}", entry.path.display()))?;
    remove_digest(&entry.path);
    if !matches!(entry.kind, Kind::Lyrics(_)) {
        skip::unmark(&target)?;
    }
    Ok(target)
}

/// The `.sha256` sidecar the archive keeps next to an archived file.
fn remove_digest(archived: &Path) {
    let mut name = archived.as_os_str().to_os_string();
    name.push(".sha256");
    let _ = fs::remove_file(PathBuf::from(name));
}

/// The tracks directly in `dir`.
pub fn tracks(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file() && scanner::is_audio(p))
                .collect()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gatefold-album-archive-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn original_names() {
        let stem = |name: &str| original_stem(Path::new(name));
        assert_eq!(stem("cover-2026-11-23.jpg").as_deref(), Some("cover"));
        assert_eq!(stem("cover-2026-11-23-2.mp4").as_deref(), Some("cover"));
        assert_eq!(
            stem("01 - Song - Live-2026-11-23.lrc").as_deref(),
            Some("01 - Song - Live")
        );
        assert_eq!(stem("cover.jpg"), None);
        assert_eq!(stem("cover-2026-11.jpg"), None);
    }

    #[test]
    fn a_plain_lrc_comes_back_under_its_tracks_name() {
        let dir = album("lyrics");
        fs::write(dir.join("01. Song.flac"), b"audio").unwrap();
        fs::write(dir.join("01. Song.lrc"), b"[00:01.00]synced").unwrap();
        let revisions = Revisions::new();
        // Archive a plain one, as a run upgrading it would.
        let plain = dir.join("plain.lrc");
        fs::write(&plain, b"plain words").unwrap();
        let archived = revisions
            .archive(
                &writer::lyrics_archive_dir(&dir),
                &plain,
                &revision::file_digest(&plain).unwrap(),
            )
            .unwrap();
        let mut renamed = archived.path().to_path_buf();
        renamed.set_file_name(
            archived
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace("plain", "01. Song"),
        );
        fs::rename(archived.path(), &renamed).unwrap();

        let entries = list(&dir);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].kind, Kind::Lyrics("01. Song".into()));
        let restored = restore(&dir, &entries[0], &revisions).unwrap();
        assert_eq!(restored, dir.join("01. Song.lrc"));
        assert_eq!(fs::read(&restored).unwrap(), b"plain words");
        // The synced one took its place in the archive.
        let entries = list(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(fs::read(&entries[0].path).unwrap(), b"[00:01.00]synced");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cover_comes_back_and_the_current_one_is_skipped() {
        let dir = album("cover");
        let covers = dir.join(ARCHIVE_DIR).join("covers");
        fs::create_dir_all(&covers).unwrap();
        fs::write(dir.join("cover.jpg"), b"\xff\xd8\xffnew").unwrap();
        fs::write(covers.join("cover-2026-01-02.png"), b"\x89PNGold").unwrap();
        let entries = list(&dir);
        assert_eq!(entries[0].kind, Kind::Cover);
        let restored = restore(&dir, &entries[0], &Revisions::new()).unwrap();
        assert_eq!(restored, dir.join("cover.png"));
        assert!(!dir.join("cover.jpg").exists());
        assert!(
            skip::rejects(&dir, b"\xff\xd8\xffnew"),
            "the replaced cover is skipped"
        );
        assert_eq!(list(&dir).len(), 1, "and archived");
        fs::remove_dir_all(&dir).unwrap();
    }
}
