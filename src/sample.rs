//! An artist-images-only run needs just each album folder's album artist, so it
//! reads a few tracks per folder instead of all of them.

use crate::scanner::Track;
use std::collections::BTreeMap;
use std::path::Path;

/// How many tracks are read from each album folder.
pub const PER_FOLDER: usize = 3;

/// The tracks grouped by folder, keeping up to [`PER_FOLDER`] from each, spread
/// across it (first, middle, last) so a guest track at one end can't win alone.
pub fn by_folder(tracks: &[Track]) -> Vec<Vec<&Track>> {
    let mut folders: BTreeMap<&Path, Vec<&Track>> = BTreeMap::new();
    for track in tracks {
        let dir = track.path.parent().unwrap_or_else(|| Path::new(""));
        folders.entry(dir).or_default().push(track);
    }
    folders.into_values().map(spread).collect()
}

fn spread(tracks: Vec<&Track>) -> Vec<&Track> {
    let n = tracks.len();
    if n <= PER_FOLDER {
        return tracks;
    }
    [0, n / 2, n - 1].into_iter().map(|i| tracks[i]).collect()
}

/// The value that occurs most often; ties go to the one seen first.
pub fn most_common<T: PartialEq + Copy>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut counts: Vec<(T, usize)> = Vec::new();
    for item in items {
        match counts.iter_mut().find(|(seen, _)| *seen == item) {
            Some((_, count)) => *count += 1,
            None => counts.push((item, 1)),
        }
    }
    // `max_by_key` keeps the last maximum, so walk in reverse to keep the first.
    counts
        .into_iter()
        .rev()
        .max_by_key(|(_, count)| *count)
        .map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(path: &str) -> Track {
        Track {
            path: PathBuf::from(path),
            has_lrc: false,
        }
    }

    fn paths<'a>(folder: &[&'a Track]) -> Vec<&'a str> {
        folder.iter().map(|t| t.path.to_str().unwrap()).collect()
    }

    #[test]
    fn keeps_first_middle_and_last_of_each_folder() {
        let tracks: Vec<Track> = ["a/1", "a/2", "a/3", "a/4", "a/5", "b/1", "b/2"]
            .into_iter()
            .map(track)
            .collect();
        let folders = by_folder(&tracks);
        assert_eq!(folders.len(), 2);
        assert_eq!(paths(&folders[0]), ["a/1", "a/3", "a/5"]);
        assert_eq!(paths(&folders[1]), ["b/1", "b/2"]);
    }

    #[test]
    fn groups_a_folder_even_when_its_tracks_are_not_contiguous() {
        let tracks: Vec<Track> = ["a/1", "a/cd1/1", "a/z"].into_iter().map(track).collect();
        let folders = by_folder(&tracks);
        assert_eq!(paths(&folders[0]), ["a/1", "a/z"]);
        assert_eq!(paths(&folders[1]), ["a/cd1/1"]);
    }

    #[test]
    fn most_common_prefers_the_majority_then_the_first_seen() {
        assert_eq!(most_common(["x", "y", "y"]), Some("y"));
        assert_eq!(most_common(["x", "y", "z"]), Some("x"));
        assert_eq!(most_common(Vec::<&str>::new()), None);
    }
}
