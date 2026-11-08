//! Hand-curated images. Empty marker files tell gatefold never to replace (or
//! archive) an existing image, even with `--replace-artist-images` /
//! `--replace-cover`; a missing one is still written, as on any run. For artists
//! and albums whose art you picked yourself (small artists, a better scan than
//! Apple Music or the Cover Art Archive have, …).
//!
//! - `.gatefold-keep` keeps every image governed by its folder: in an artist
//!   folder, the artist images *and* the album covers below it; in an album
//!   folder, that album's cover.
//! - Without it, granular markers keep one kind each: `.gatefold-primary-keep`
//!   (`folder.jpg`), `.gatefold-backdrop-keep` (`backdrop.jpg`),
//!   `.gatefold-video-keep` (`backdrops/backdrop.mp4`), `.gatefold-logo-keep`
//!   (`logo.png`) and `.gatefold-cover-keep` (embedded covers + `cover.jpg`, in an
//!   album folder or any folder above it).
//!
//! Lyrics are never affected. `gatefold keep` writes these markers for you.

use crate::skip::{self, Managed};
use crate::writer::ArtistImage;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// Keeps everything its folder governs.
pub const ALL: &str = ".gatefold-keep";
/// Keeps the album cover (embedded + `cover.jpg`) of this folder and those below.
pub const COVER: &str = ".gatefold-cover-keep";

/// Every artist file, for "is all of it kept?" checks.
pub const ARTIST_IMAGES: [ArtistImage; 4] = [
    ArtistImage::Avatar,
    ArtistImage::Backdrop,
    ArtistImage::Video,
    ArtistImage::Logo,
];

/// The granular marker for one artist file. Named after the archive folders
/// (`primary/`, `backdrops/`) and Jellyfin's image types, not the file names.
pub fn artist_marker(kind: ArtistImage) -> &'static str {
    match kind {
        ArtistImage::Avatar => ".gatefold-primary-keep",
        ArtistImage::Backdrop => ".gatefold-backdrop-keep",
        ArtistImage::Video => ".gatefold-video-keep",
        ArtistImage::Logo => ".gatefold-logo-keep",
    }
}

fn has(dir: &Path, marker: &str) -> bool {
    dir.join(marker).is_file()
}

/// True when artist folder `dir` keeps its `kind` file as is.
pub fn artist_image(dir: &Path, kind: ArtistImage) -> bool {
    has(dir, ALL) || has(dir, artist_marker(kind))
}

/// True when artist folder `dir` keeps every artist file as is.
pub fn all_artist_images(dir: &Path) -> bool {
    ARTIST_IMAGES.iter().all(|kind| artist_image(dir, *kind))
}

/// True when the cover of the album in `album_dir` is kept as is: the album
/// folder, or any folder above it within the scan `root` (the artist folder),
/// holds `.gatefold-keep` or `.gatefold-cover-keep`.
pub fn cover(album_dir: &Path, root: &Path) -> bool {
    album_dir
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .any(|dir| has(dir, ALL) || has(dir, COVER))
}

/// What writing a keep marker did.
#[derive(Debug, PartialEq, Eq)]
pub enum Marked {
    Created(PathBuf),
    AlreadyKept(PathBuf),
}

impl Marked {
    pub fn path(&self) -> &Path {
        match self {
            Marked::Created(p) | Marked::AlreadyKept(p) => p,
        }
    }
}

fn write_marker(dir: &Path, name: &str) -> Result<Marked> {
    let marker = dir.join(name);
    if marker.is_file() {
        return Ok(Marked::AlreadyKept(marker));
    }
    fs::write(&marker, b"").with_context(|| format!("writing {}", marker.display()))?;
    Ok(Marked::Created(marker))
}

/// Keep artist folder `dir`'s `kind` file: write its granular marker.
pub fn mark_artist_image(dir: &Path, kind: ArtistImage) -> Result<Marked> {
    write_marker(dir, artist_marker(kind))
}

/// What `gatefold keep` did for one path.
#[derive(Debug, PartialEq, Eq)]
pub struct Kept {
    pub marker: Marked,
    /// A skip marker for the kept image's contents, removed: keeping it means
    /// you want it after all.
    pub unskipped: Option<PathBuf>,
}

/// `gatefold keep --dir <dir>`: write `.gatefold-keep`, keeping everything the
/// folder governs.
pub fn mark_dir(dir: &Path) -> Result<Kept> {
    if !dir.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    Ok(Kept {
        marker: write_marker(dir, ALL)?,
        unskipped: None,
    })
}

/// `gatefold keep --file <image>`: write the granular marker for the image's
/// kind, in the folder that governs it (`.gatefold-cover-keep` in the album
/// folder for `cover.*`, `.gatefold-video-keep` in the artist folder for
/// `backdrops/backdrop.mp4`, …).
pub fn mark_file(path: &Path) -> Result<Kept> {
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    let Some((managed, dir)) = skip::classify(path) else {
        bail!(
            "{} is not an image gatefold writes (cover.jpg, folder.jpg, backdrop.jpg, \
             backdrops/backdrop.mp4 or logo.png); use --dir to keep everything in a folder",
            path.display()
        );
    };
    let marker = match managed {
        Managed::Cover => write_marker(&dir, COVER)?,
        Managed::Artist(kind) => mark_artist_image(&dir, kind)?,
    };
    Ok(Kept {
        marker,
        unskipped: skip::unmark(path)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("gatefold-keep-{name}-{}", std::process::id()));
        let artist = root.join("Kemuri");
        let album = artist.join("1997 - Little Playmate");
        fs::create_dir_all(&album).unwrap();
        (root, artist, album)
    }

    #[test]
    fn nothing_is_kept_without_markers() {
        let (root, artist, album) = tree("none");
        assert!(ARTIST_IMAGES.iter().all(|k| !artist_image(&artist, *k)));
        assert!(!all_artist_images(&artist));
        assert!(!cover(&album, &root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn keep_in_the_artist_folder_keeps_everything_below() {
        let (root, artist, album) = tree("all");
        fs::write(artist.join(ALL), b"").unwrap();
        assert!(all_artist_images(&artist));
        assert!(cover(&album, &root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn granular_markers_keep_one_kind() {
        let (root, artist, album) = tree("granular");
        fs::write(artist.join(".gatefold-primary-keep"), b"").unwrap();
        assert!(artist_image(&artist, ArtistImage::Avatar));
        assert!(!artist_image(&artist, ArtistImage::Backdrop));
        assert!(!artist_image(&artist, ArtistImage::Video));
        assert!(!artist_image(&artist, ArtistImage::Logo));
        assert!(!all_artist_images(&artist));
        assert!(!cover(&album, &root));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cover_markers_apply_to_the_album_and_below_the_artist() {
        let (root, artist, album) = tree("cover");
        let other = artist.join("1998 - 77 Days");
        fs::create_dir_all(&other).unwrap();

        // In one album folder: only that album.
        fs::write(album.join(COVER), b"").unwrap();
        assert!(cover(&album, &root));
        assert!(!cover(&other, &root));
        assert!(!all_artist_images(&artist));

        // In the artist folder: every album, but not the artist images.
        fs::write(artist.join(COVER), b"").unwrap();
        assert!(cover(&other, &root));
        assert!(!artist_image(&artist, ArtistImage::Avatar));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn keeping_writes_the_marker_for_the_images_kind() {
        let (root, artist, album) = tree("mark");
        fs::create_dir_all(artist.join("backdrops")).unwrap();
        for file in ["folder.jpg", "backdrops/backdrop.mp4"] {
            fs::write(artist.join(file), file.as_bytes()).unwrap();
        }
        fs::write(album.join("cover.jpg"), b"scan").unwrap();

        let kept = mark_file(&artist.join("folder.jpg")).unwrap();
        assert_eq!(
            kept.marker,
            Marked::Created(artist.join(".gatefold-primary-keep"))
        );
        assert_eq!(kept.unskipped, None);
        assert!(artist_image(&artist, ArtistImage::Avatar));
        assert!(!artist_image(&artist, ArtistImage::Backdrop));
        assert_eq!(
            mark_file(&artist.join("folder.jpg")).unwrap().marker,
            Marked::AlreadyKept(artist.join(".gatefold-primary-keep"))
        );

        mark_file(&artist.join("backdrops/backdrop.mp4")).unwrap();
        assert!(artist.join(".gatefold-video-keep").is_file());

        // Keeping a skipped cover takes the skip back.
        skip::mark(&album.join("cover.jpg")).unwrap();
        let kept = mark_file(&album.join("cover.jpg")).unwrap();
        assert_eq!(kept.marker, Marked::Created(album.join(COVER)));
        assert!(kept.unskipped.is_some());
        assert!(!skip::cover(&album));
        assert!(cover(&album, &root));

        // A folder keeps everything it governs.
        let kept = mark_dir(&artist).unwrap();
        assert_eq!(kept.marker, Marked::Created(artist.join(ALL)));
        assert!(all_artist_images(&artist));

        assert!(mark_file(&album.join("01. Song.flac")).is_err());
        assert!(mark_file(&artist).is_err(), "a folder needs --dir");
        assert!(
            mark_dir(&artist.join("folder.jpg")).is_err(),
            "a file needs --file"
        );
        fs::write(album.join("01. Song.flac"), b"x").unwrap();
        assert!(mark_file(&album.join("01. Song.flac")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn markers_above_the_root_are_ignored() {
        let (root, artist, album) = tree("root");
        fs::write(root.join(ALL), b"").unwrap();
        // Scanning from the artist folder: the marker in its parent doesn't count.
        assert!(!cover(&album, &artist));
        fs::remove_dir_all(&root).unwrap();
    }
}
