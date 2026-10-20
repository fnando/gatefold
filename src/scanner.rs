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

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
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
    tracks.sort_by(|a, b| {
        natord::compare(&a.path.to_string_lossy(), &b.path.to_string_lossy())
    });
    tracks
}
