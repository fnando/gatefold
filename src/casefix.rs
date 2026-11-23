//! Sidecars whose names differ from what gatefold (and Jellyfin, on a disk
//! that minds case) expect only in case: `01. song.lrc` next to
//! `01. Song.flac`, `Cover.JPG`. A library edited on macOS, which ignores case,
//! and processed on Linux, which doesn't, ends up with those, and often with
//! both spellings side by side.
//!
//! [`scan`] finds them; [`fix`] keeps one file under the expected name and
//! moves any other spelling into the folder's `.gatefold-archive/`, dated.

use crate::revision::{self, Revisions};
use crate::scanner;
use crate::writer::{self, ARCHIVE_DIR, ArtistImage};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A file name to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// What the file should be called.
    pub expected: PathBuf,
    /// The files spelled differently (and the expected one, when it's there).
    pub found: Vec<PathBuf>,
}

impl Problem {
    /// Several spellings side by side, rather than one misnamed file.
    pub fn duplicate(&self) -> bool {
        self.found.len() > 1
    }

    pub fn describe(&self) -> String {
        let names: Vec<String> = self.found.iter().map(|p| name(p)).collect();
        if self.duplicate() {
            format!(
                "{} are the same file in different case",
                names.join(" and ")
            )
        } else {
            format!("{} should be {}", names.join(""), name(&self.expected))
        }
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The names gatefold writes (lowercase), besides each track's `.lrc`.
fn managed_names() -> Vec<&'static str> {
    let mut names = vec!["cover.jpg", "cover.jpeg", "cover.png", writer::COVER_VIDEO];
    for kind in [
        ArtistImage::Avatar,
        ArtistImage::Backdrop,
        ArtistImage::Logo,
    ] {
        names.extend(kind.names().iter().copied());
    }
    names
}

/// Problems in `dir` itself.
pub fn scan_dir(dir: &Path) -> Vec<Problem> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    // The expected spelling of every name gatefold manages here: each track's
    // `<stem>.lrc`, and the image names (lowercase).
    let mut expected: BTreeMap<String, String> = managed_names()
        .into_iter()
        .map(|n| (n.to_string(), n.to_string()))
        .collect();
    for track in files.iter().filter(|p| scanner::is_audio(p)) {
        if let Some(stem) = track.file_stem().and_then(|s| s.to_str()) {
            let lrc = format!("{stem}.lrc");
            expected.insert(lrc.to_lowercase(), lrc);
        }
    }
    let mut by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in &files {
        let lower = name(file).to_lowercase();
        if expected.contains_key(&lower) {
            by_name.entry(lower).or_default().push(file.clone());
        }
    }
    by_name
        .into_iter()
        .filter_map(|(lower, mut found)| {
            let expected = dir.join(&expected[&lower]);
            let misnamed = found.len() == 1 && found[0] != expected;
            (found.len() > 1 || misnamed).then(|| {
                found.sort();
                Problem { expected, found }
            })
        })
        .collect()
}

/// Problems anywhere under `root` (never inside `.gatefold-archive/`).
pub fn scan(root: &Path) -> Vec<Problem> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ARCHIVE_DIR)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir())
        .flat_map(|e| scan_dir(e.path()))
        .collect()
}

/// What fixing a problem did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixed {
    /// The file now under the expected name.
    pub kept: PathBuf,
    /// The other spellings, and where they went.
    pub archived: Vec<(PathBuf, PathBuf)>,
}

/// Keep the best of the spellings under the expected name; archive the rest.
/// For lyrics the best is a synced one (newest first); for images, the one
/// already under the expected name, else the newest.
pub fn fix(problem: &Problem, revisions: &Revisions) -> Result<Fixed> {
    let dir = problem
        .expected
        .parent()
        .context("a file without a folder")?;
    let lyrics = problem
        .expected
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lrc"));
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut found = problem.found.clone();
    found.sort_by_key(|p| {
        (
            lyrics && writer::lrc_is_unsynced(p),
            !lyrics && *p != problem.expected,
            std::cmp::Reverse(modified(p)),
        )
    });
    let best = found.remove(0);

    let archive_dir = if lyrics {
        writer::lyrics_archive_dir(dir)
    } else {
        dir.join(ARCHIVE_DIR).join("case")
    };
    let mut archived = Vec::new();
    for other in found {
        // On a disk that ignores case, the expected name may be this very
        // file under another spelling: only archive a real second file.
        if same_file(&other, &best) {
            continue;
        }
        let digest = revision::file_digest(&other)?;
        let to = revisions.archive(&archive_dir, &other, &digest)?;
        archived.push((other, to.path().to_path_buf()));
    }
    if best != problem.expected {
        rename_case(&best, &problem.expected)?;
    }
    Ok(Fixed {
        kept: problem.expected.clone(),
        archived,
    })
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Rename through a temporary name, so a change of case alone works on a
/// disk that ignores case too.
fn rename_case(from: &Path, to: &Path) -> Result<()> {
    let temp = from.with_file_name(format!(".{}.gatefold-case", name(from)));
    fs::rename(from, &temp).with_context(|| format!("renaming {}", from.display()))?;
    fs::rename(&temp, to).with_context(|| format!("renaming to {}", to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gatefold-casefix-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Whether this disk tells `a` from `A` (macOS usually doesn't).
    fn minds_case(dir: &Path) -> bool {
        fs::write(dir.join("probe"), b"").unwrap();
        let minds = !dir.join("PROBE").exists();
        fs::remove_file(dir.join("probe")).unwrap();
        minds
    }

    #[test]
    fn a_misnamed_lyrics_file_gets_the_tracks_name() {
        let dir = dir("misnamed");
        fs::write(dir.join("01. Song.flac"), b"audio").unwrap();
        fs::write(dir.join("01. song.lrc"), b"[00:01.00]hi").unwrap();
        fs::write(dir.join("Cover.JPG"), b"cover").unwrap();
        let problems = scan(&dir);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|p| !p.duplicate()));
        let revisions = Revisions::new();
        for problem in &problems {
            fix(problem, &revisions).unwrap();
        }
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| name(&e.unwrap().path()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        assert!(names.contains(&"01. Song.lrc".to_string()), "{names:?}");
        assert!(names.contains(&"cover.jpg".to_string()), "{names:?}");
        assert_eq!(scan(&dir), []);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicates_keep_the_synced_lyrics() {
        let dir = dir("duplicate");
        if !minds_case(&dir) {
            fs::remove_dir_all(&dir).unwrap();
            return; // can't have both spellings here
        }
        fs::write(dir.join("01. Song.flac"), b"audio").unwrap();
        fs::write(dir.join("01. Song.lrc"), b"plain words").unwrap();
        fs::write(dir.join("01. song.lrc"), b"[00:01.00]synced").unwrap();
        let problems = scan(&dir);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].duplicate());
        let fixed = fix(&problems[0], &Revisions::new()).unwrap();
        assert_eq!(fixed.archived.len(), 1);
        assert_eq!(
            fs::read(dir.join("01. Song.lrc")).unwrap(),
            b"[00:01.00]synced"
        );
        assert!(!dir.join("01. song.lrc").exists());
        assert!(
            fixed.archived[0]
                .1
                .starts_with(writer::lyrics_archive_dir(&dir))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lookups_ignore_case() {
        let dir = dir("lookup");
        fs::write(dir.join("01. Song.flac"), b"audio").unwrap();
        fs::write(dir.join("01. SONG.lrc"), b"x").unwrap();
        fs::write(dir.join("Cover.Png"), b"x").unwrap();
        // Found under either spelling (a disk that ignores case answers to both).
        let lrc = scanner::lrc_path_for(&dir.join("01. Song.flac"));
        assert!(lrc.is_file(), "{}", lrc.display());
        assert!(name(&lrc).eq_ignore_ascii_case("01. song.lrc"));
        assert!(writer::folder_has_cover(&dir));
        assert!(writer::existing_cover_sidecar(&dir).is_some_and(|p| p.is_file()));
        assert_eq!(crate::paths::find_ignoring_case(&dir, "nope.jpg"), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
