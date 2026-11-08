//! Rejected images. `gatefold skip <image>` drops an empty
//! `.gatefold-<sha256>-skip` marker next to the image, naming its contents:
//!
//! - an existing file with those contents counts as missing, so the next run
//!   replaces it with a different image as soon as one is available (the old one
//!   is archived / backed up as on any replace), even without `--replace-*` and
//!   even under a keep marker;
//! - a downloaded image with those contents is never written into that folder.
//!
//! Unlike a keep marker, a skip marker doesn't freeze the folder: any other image
//! is welcome. Delete the marker to take the image back.

use crate::state::digest_bytes;
use crate::writer::ArtistImage;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const PREFIX: &str = ".gatefold-";
const SUFFIX: &str = "-skip";

/// Album cover sidecars `gatefold skip` accepts (what `--replace-cover` replaces).
const COVER_NAMES: &[&str] = &["cover.jpg", "cover.jpeg", "cover.png"];

/// The marker rejecting contents with this SHA-256 hex digest.
pub fn marker_name(digest: &str) -> String {
    format!("{PREFIX}{digest}{SUFFIX}")
}

/// True when `dir` holds any skip marker (so hashing is worth it).
fn has_markers(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.len() > PREFIX.len() + SUFFIX.len()
            && name.starts_with(PREFIX)
            && name.ends_with(SUFFIX)
            && entry.path().is_file()
    })
}

/// True when `dir` rejects an image with these bytes.
pub fn rejects(dir: &Path, data: &[u8]) -> bool {
    has_markers(dir) && dir.join(marker_name(&digest_bytes(data))).is_file()
}

/// True when the existing `file` is rejected by a marker next to it.
pub fn rejected(file: &Path) -> bool {
    let Some(dir) = file.parent() else {
        return false;
    };
    has_markers(dir) && fs::read(file).is_ok_and(|bytes| rejects(dir, &bytes))
}

/// True when album folder `dir` has a `cover.*` sidecar that was skipped.
pub fn cover(dir: &Path) -> bool {
    COVER_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
        .is_some_and(|path| rejected(&path))
}

/// The `kind` file in artist folder `dir`, unless there is none or it was skipped.
pub fn present(dir: &Path, kind: ArtistImage) -> Option<PathBuf> {
    kind.existing(dir).filter(|path| !rejected(path))
}

/// True when the `kind` file in artist folder `dir` exists but was skipped.
pub fn artist_image(dir: &Path, kind: ArtistImage) -> bool {
    kind.existing(dir).is_some_and(|path| rejected(&path))
}

/// What `mark` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Marked {
    Created(PathBuf),
    AlreadySkipped(PathBuf),
}

/// Reject the contents of `image` (an album cover or artist file gatefold
/// manages) by writing a marker next to it. The image itself is left in place
/// until a run finds something to replace it with.
pub fn mark(image: &Path) -> Result<Marked> {
    if !image.is_file() {
        bail!("{} is not a file", image.display());
    }
    if !managed(image) {
        bail!(
            "{} is not an image gatefold writes (cover.jpg, folder.jpg, backdrop.jpg, \
             backdrops/backdrop.mp4 or logo.png)",
            image.display()
        );
    }
    let bytes = fs::read(image).with_context(|| format!("reading {}", image.display()))?;
    let dir = image.parent().unwrap_or_else(|| Path::new("."));
    let marker = dir.join(marker_name(&digest_bytes(&bytes)));
    if marker.is_file() {
        return Ok(Marked::AlreadySkipped(marker));
    }
    fs::write(&marker, b"").with_context(|| format!("writing {}", marker.display()))?;
    Ok(Marked::Created(marker))
}

/// True for the files a run would replace: album cover sidecars and artist
/// files (a motion backdrop only inside `backdrops/`). Anything else (archived
/// versions, audio) would get a marker no run ever reads.
fn managed(image: &Path) -> bool {
    let Some(name) = image.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let in_backdrops = image
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "backdrops");
    COVER_NAMES.contains(&name.as_str())
        || [
            ArtistImage::Avatar,
            ArtistImage::Backdrop,
            ArtistImage::Logo,
        ]
        .iter()
        .any(|kind| kind.names().contains(&name.as_str()))
        || (in_backdrops && ArtistImage::Video.names().contains(&name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gatefold-skip-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nothing_is_rejected_without_markers() {
        let dir = dir("none");
        fs::write(dir.join("folder.jpg"), b"photo").unwrap();
        assert!(!rejects(&dir, b"photo"));
        assert!(!rejected(&dir.join("folder.jpg")));
        assert!(present(&dir, ArtistImage::Avatar).is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn marking_rejects_those_bytes_only() {
        let dir = dir("mark");
        let photo = dir.join("folder.jpg");
        fs::write(&photo, b"photo").unwrap();
        let marker = dir.join(marker_name(&digest_bytes(b"photo")));
        assert_eq!(mark(&photo).unwrap(), Marked::Created(marker.clone()));
        assert_eq!(mark(&photo).unwrap(), Marked::AlreadySkipped(marker));
        assert!(
            photo.is_file(),
            "the image stays until something replaces it"
        );

        assert!(rejects(&dir, b"photo"));
        assert!(!rejects(&dir, b"another photo"));
        assert!(artist_image(&dir, ArtistImage::Avatar));
        assert!(present(&dir, ArtistImage::Avatar).is_none());

        // A different image in its place is welcome.
        fs::write(&photo, b"another photo").unwrap();
        assert!(!artist_image(&dir, ArtistImage::Avatar));
        assert!(present(&dir, ArtistImage::Avatar).is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skipped_cover_sidecar() {
        let dir = dir("cover");
        assert!(!cover(&dir));
        fs::write(dir.join("cover.jpg"), b"scan").unwrap();
        assert!(!cover(&dir));
        mark(&dir.join("cover.jpg")).unwrap();
        assert!(cover(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_managed_files_can_be_skipped() {
        let dir = dir("managed");
        let backdrops = dir.join("backdrops");
        fs::create_dir_all(&backdrops).unwrap();
        for name in ["track.flac", "backdrop.mp4", "folder-2026-01-01.jpg"] {
            fs::write(dir.join(name), b"x").unwrap();
            assert!(mark(&dir.join(name)).is_err(), "{name} should be refused");
        }
        fs::write(backdrops.join("backdrop.mp4"), b"video").unwrap();
        assert!(mark(&backdrops.join("backdrop.mp4")).is_ok());
        assert!(artist_image(&dir, ArtistImage::Video));
        assert!(mark(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
