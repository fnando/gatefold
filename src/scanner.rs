use crate::writer::ArtistImage;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Audio extensions we attempt to read/write lyrics for (lowercased).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "m4a", "mp4", "wav", "aac", "wma",
];

/// A music file discovered during the scan, with whether a sibling `.lrc` exists.
#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub has_lrc: bool,
}

/// The `<stem>.lrc` path that sits alongside a given track.
pub fn lrc_path_for(path: &Path) -> PathBuf {
    path.with_extension("lrc")
}

/// True when `path` has one of the audio extensions we handle. Motion backdrops
/// share the `.mp4` extension with audio containers, so anything inside a
/// `backdrops/` folder (Jellyfin's theme-video folder, where gatefold writes them
/// and keeps replaced ones) or named like one is excluded.
pub fn is_audio(path: &Path) -> bool {
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    let in_backdrops = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("backdrops"));
    let is_backdrop_video = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        let lower = n.to_ascii_lowercase();
        ArtistImage::Video.names().contains(&lower.as_str())
    });
    by_extension && !in_backdrops && !is_backdrop_video
}

/// True when `dir` directly contains at least one audio file (not recursive).
/// Artist-image placement relies on this: a folder that holds tracks is an album
/// folder, where `folder.jpg` would be read as the album cover (see
/// `writer::COVER_FILENAMES`), so it's never used as an artist folder.
pub fn dir_has_audio(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|e| e.file_type().is_ok_and(|t| t.is_file()) && is_audio(&e.path()))
    })
}

/// Recursively collect audio files under `root`, sorted for deterministic order.
pub fn scan(root: &Path) -> Vec<Track> {
    let mut tracks: Vec<Track> = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| is_audio(path))
        .map(|path| {
            let has_lrc = lrc_path_for(&path).is_file();
            Track { path, has_lrc }
        })
        .collect();

    // Natural order so numeric runs sort by value (1, 2, … 10) rather than
    // lexically (1, 10, 2). Compare the full path string so folder grouping holds.
    tracks.sort_by(|a, b| natord::compare(&a.path.to_string_lossy(), &b.path.to_string_lossy()));
    tracks
}
