//! `gatefold archive --file <artist file>`: move an artist file (`folder.jpg`,
//! `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`) into the artist
//! folder's hidden `.gatefold-archive/`, dated, as a replace run would. The
//! file is skipped first (`gatefold skip`), so later runs don't write the same
//! contents back but take a different image as soon as one is available.
//! `gatefold restore` brings it back.

use crate::revision::{self, Archived, Revisions};
use crate::skip::{self, Managed};
use crate::writer::ArtistImage;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// What archiving one file did.
#[derive(Debug, PartialEq, Eq)]
pub struct Moved {
    /// The artist folder the file belongs to.
    pub dir: PathBuf,
    /// Where it went, relative to the artist folder, with ` (already kept)`
    /// when the archive already held the same contents.
    pub archived: String,
}

/// Skip `current`, the `kind` file of artist folder `dir`, then move it into
/// the archive. Returns where it went, relative to `dir`.
pub fn skip_and_archive(
    dir: &Path,
    kind: ArtistImage,
    current: &Path,
    revisions: &Revisions,
) -> Result<String> {
    let digest = revision::file_digest(current)?;
    skip::mark(current)?;
    let archived = revisions.archive(&kind.archive_dir(dir), current, &digest)?;
    let relative = archived
        .path()
        .strip_prefix(dir)
        .unwrap_or(archived.path())
        .to_string_lossy()
        .into_owned();
    Ok(match archived {
        Archived::Moved(_) => relative,
        Archived::Duplicate(_) => format!("{relative} (already kept)"),
    })
}

/// Archive the artist file at `path`.
pub fn file(path: &Path, revisions: &Revisions) -> Result<Moved> {
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    let Some((Managed::Artist(kind), dir)) = skip::classify(path) else {
        bail!(
            "{} is not an artist file gatefold writes (folder.jpg, backdrop.jpg, \
             backdrops/backdrop.mp4 or logo.png)",
            path.display()
        );
    };
    let archived = skip_and_archive(&dir, kind, path, revisions)?;
    Ok(Moved { dir, archived })
}

/// `gatefold archive --file …`: archive each file. Every file is attempted; the
/// command fails if any of them couldn't be archived.
pub fn run(files: &[PathBuf]) -> Result<()> {
    let revisions = Revisions::new();
    let mut failed = 0;
    for path in files {
        match file(path, &revisions) {
            Ok(moved) => println!(
                "✓ {}: skipped and archived as {}",
                path.display(),
                moved.dir.join(&moved.archived).display()
            ),
            Err(err) => {
                failed += 1;
                eprintln!("✗ {err:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} file(s) could not be archived");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::ARCHIVE_DIR;
    use std::fs;

    fn artist(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gatefold-archive-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("backdrops")).unwrap();
        dir
    }

    #[test]
    fn archives_and_skips_a_backdrop() {
        let dir = artist("backdrop");
        let revisions = Revisions::new();
        let date = revisions.date();
        fs::write(dir.join("backdrop.jpg"), b"wide").unwrap();

        let moved = file(&dir.join("backdrop.jpg"), &revisions).unwrap();
        let archived = format!("{ARCHIVE_DIR}/backdrops/backdrop-{date}.jpg");
        assert_eq!(
            moved,
            Moved {
                dir: dir.clone(),
                archived: archived.clone()
            }
        );
        assert!(!dir.join("backdrop.jpg").exists());
        assert!(!dir.join("backdrop.jpg.sha256").exists());
        assert_eq!(fs::read(dir.join(&archived)).unwrap(), b"wide");
        assert!(
            skip::rejects(&dir, b"wide"),
            "later runs don't write it back"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archives_a_motion_backdrop_and_drops_duplicates() {
        let dir = artist("video");
        let revisions = Revisions::new();
        let video = dir.join("backdrops/backdrop.mp4");
        fs::write(&video, b"loop").unwrap();
        let first = file(&video, &revisions).unwrap();
        assert!(
            first
                .archived
                .starts_with(&format!("{ARCHIVE_DIR}/backdrops/backdrop-"))
        );
        assert!(skip::rejects(&dir.join("backdrops"), b"loop"));

        // The same contents again: removed, pointing at the copy already archived.
        fs::write(&video, b"loop").unwrap();
        let again = file(&video, &revisions).unwrap();
        assert_eq!(again.archived, format!("{} (already kept)", first.archived));
        assert!(!video.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_covers_and_other_files() {
        let dir = artist("refuse");
        for name in ["cover.jpg", "notes.txt"] {
            fs::write(dir.join(name), b"x").unwrap();
            assert!(file(&dir.join(name), &Revisions::new()).is_err());
            assert!(dir.join(name).exists());
        }
        assert!(file(&dir.join("folder.jpg"), &Revisions::new()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
