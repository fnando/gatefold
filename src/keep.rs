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
//!   `.gatefold-video-keep` (`backdrops/backdrop.mp4`) and `.gatefold-cover-keep`
//!   (embedded covers + `cover.jpg`, in an album folder or any folder above it).
//!
//! Lyrics are never affected.

use crate::writer::ArtistImage;
use std::path::Path;

/// Keeps everything its folder governs.
pub const ALL: &str = ".gatefold-keep";
/// Keeps the album cover (embedded + `cover.jpg`) of this folder and those below.
pub const COVER: &str = ".gatefold-cover-keep";

/// Every artist file, for "is all of it kept?" checks.
pub const ARTIST_IMAGES: [ArtistImage; 3] = [
    ArtistImage::Avatar,
    ArtistImage::Backdrop,
    ArtistImage::Video,
];

/// The granular marker for one artist file. Named after the archive folders
/// (`primary/`, `backdrops/`) and Jellyfin's image types, not the file names.
pub fn artist_marker(kind: ArtistImage) -> &'static str {
    match kind {
        ArtistImage::Avatar => ".gatefold-primary-keep",
        ArtistImage::Backdrop => ".gatefold-backdrop-keep",
        ArtistImage::Video => ".gatefold-video-keep",
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
    fn markers_above_the_root_are_ignored() {
        let (root, artist, album) = tree("root");
        fs::write(root.join(ALL), b"").unwrap();
        // Scanning from the artist folder: the marker in its parent doesn't count.
        assert!(!cover(&album, &artist));
        fs::remove_dir_all(&root).unwrap();
    }
}
