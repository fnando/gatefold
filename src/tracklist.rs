//! `gatefold tracklist --dir <album folder>`: print an album's tracks, one per
//! line, as `<track>. <title> <duration>`, read from the tags of the audio
//! files directly inside the folder (not below it).

use crate::scanner;
use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::read_from_path;
use lofty::tag::Accessor;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One track as the tracklist shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub title: String,
    pub duration: Duration,
}

/// Print the tracklist of `dir`. Files whose tags can't be read are reported
/// on stderr and left out.
pub fn run(dir: &Path) -> Result<()> {
    crate::paths::require_dir(dir, None)?;
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| scanner::is_audio(p))
        .collect();
    files.sort_by(|a, b| natord::compare(&a.to_string_lossy(), &b.to_string_lossy()));

    let mut entries = Vec::new();
    for path in &files {
        match read(path) {
            Ok(entry) => entries.push(entry),
            Err(err) => eprintln!("✗ {err:#}"),
        }
    }
    for line in render(entries) {
        println!("{line}");
    }
    Ok(())
}

fn read(path: &Path) -> Result<Entry> {
    let tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag
        .and_then(Accessor::title)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
        });
    Ok(Entry {
        disc: tag.and_then(Accessor::disk),
        track: tag.and_then(Accessor::track),
        title,
        duration: tagged.properties().duration(),
    })
}

/// Order the entries by disc and track (files without a number keep their
/// filename order, after the numbered ones) and format them. A track without a
/// number is numbered by its position. Albums with more than one disc get a
/// `Disc N` heading before each disc.
pub fn render(mut entries: Vec<Entry>) -> Vec<String> {
    // Stable sort: the incoming (filename) order breaks ties.
    entries.sort_by_key(|e| (e.disc.unwrap_or(1), e.track.unwrap_or(u32::MAX)));
    let discs = {
        let mut discs: Vec<u32> = entries.iter().map(|e| e.disc.unwrap_or(1)).collect();
        discs.dedup();
        discs
    };
    let multi_disc = discs.len() > 1;

    let mut lines = Vec::new();
    let mut current_disc = None;
    let mut position = 0;
    for entry in &entries {
        let disc = entry.disc.unwrap_or(1);
        if current_disc != Some(disc) {
            if multi_disc {
                if current_disc.is_some() {
                    lines.push(String::new());
                }
                lines.push(format!("Disc {disc}"));
            }
            current_disc = Some(disc);
            position = 0;
        }
        position += 1;
        let number = entry.track.unwrap_or(position);
        lines.push(format!(
            "{number}. {} {}",
            entry.title,
            format_duration(entry.duration)
        ));
    }
    lines
}

/// `m:ss`, or `h:mm:ss` from an hour up, rounded to the nearest second.
pub fn format_duration(duration: Duration) -> String {
    let total = (duration.as_millis() + 500) / 1000;
    let (hours, minutes, seconds) = (total / 3600, total % 3600 / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(disc: Option<u32>, track: Option<u32>, title: &str, secs: u64) -> Entry {
        Entry {
            disc,
            track,
            title: title.to_string(),
            duration: Duration::from_secs(secs),
        }
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::ZERO), "0:00");
        assert_eq!(format_duration(Duration::from_millis(59_499)), "0:59");
        assert_eq!(format_duration(Duration::from_millis(59_500)), "1:00");
        assert_eq!(format_duration(Duration::from_secs(245)), "4:05");
        assert_eq!(format_duration(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn orders_by_track_number() {
        let lines = render(vec![
            entry(None, Some(2), "Second", 61),
            entry(None, Some(10), "Tenth", 200),
            entry(None, Some(1), "First", 5),
        ]);
        assert_eq!(lines, ["1. First 0:05", "2. Second 1:01", "10. Tenth 3:20"]);
    }

    #[test]
    fn numbers_untagged_tracks_by_position() {
        let lines = render(vec![entry(None, None, "a", 1), entry(None, None, "b", 2)]);
        assert_eq!(lines, ["1. a 0:01", "2. b 0:02"]);
    }

    #[test]
    fn heads_each_disc_when_there_are_several() {
        let lines = render(vec![
            entry(Some(2), Some(1), "B1", 1),
            entry(Some(1), Some(1), "A1", 1),
        ]);
        assert_eq!(lines, ["Disc 1", "1. A1 0:01", "", "Disc 2", "1. B1 0:01"]);
    }

    #[test]
    fn reads_a_real_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/joint-credit.flac");
        let entry = read(&path).unwrap();
        assert_ne!(entry.title, "");
    }
}
