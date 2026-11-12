//! `gatefold restore`: bring back an artist file from the artist folder's
//! `.gatefold-archive/`. The file it replaces is skipped (`gatefold skip`) and
//! archived, so later runs neither keep it nor write it back; the restored
//! image loses any skip marker of its own. `--keep` also writes its keep
//! marker, so replace runs leave it alone.

use crate::archive;
use crate::revision::{self, Revisions};
use crate::writer::{ARCHIVE_DIR, ArtistImage};
use crate::{keep, skip};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// An archived version of an artist file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: ArtistImage,
    pub path: PathBuf,
}

/// What `restore` did.
#[derive(Debug, PartialEq, Eq)]
pub struct Restored {
    /// The restored file, now current.
    pub current: PathBuf,
    /// The file it replaced (skipped), and where it was archived, relative to
    /// the artist folder.
    pub replaced: Option<(PathBuf, String)>,
}

/// Archive subfolders, in menu order, with the kinds archived there.
const SECTIONS: [(&str, &[ArtistImage]); 3] = [
    ("primary", &[ArtistImage::Avatar]),
    ("backdrops", &[ArtistImage::Backdrop, ArtistImage::Video]),
    ("logos", &[ArtistImage::Logo]),
];

/// The kind of artist file an archived `path` is a version of, by its folder
/// under `.gatefold-archive/` and its extension.
fn kind_of(path: &Path) -> Option<ArtistImage> {
    let sub = path.parent()?.file_name()?.to_str()?;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (_, kinds) = SECTIONS.iter().find(|(name, _)| *name == sub)?;
    kinds.iter().copied().find(|kind| {
        revision::is_archived_name(path, kind.stem())
            && kind
                .names()
                .iter()
                .any(|name| name.rsplit('.').next() == Some(ext.as_str()))
    })
}

/// The artist folder an archived `path` belongs to:
/// `<artist>/.gatefold-archive/<sub>/<file>`.
fn artist_dir_of(path: &Path) -> Option<&Path> {
    let archive = path.parent()?.parent()?;
    (archive.file_name()? == ARCHIVE_DIR).then(|| archive.parent())?
}

/// Every archived version in artist folder `dir`: avatars, backdrops (still,
/// then motion) and logos, newest first within each.
pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    let archive = dir.join(ARCHIVE_DIR);
    if !archive.is_dir() {
        bail!("{} has no {ARCHIVE_DIR}/ folder", dir.display());
    }
    let mut entries = Vec::new();
    for (sub, kinds) in SECTIONS {
        let Ok(files) = fs::read_dir(archive.join(sub)) else {
            continue;
        };
        let mut found: Vec<Entry> = files
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter_map(|path| kind_of(&path).map(|kind| Entry { kind, path }))
            .collect();
        found.sort_by(|a, b| {
            let rank = |k: ArtistImage| kinds.iter().position(|x| *x == k);
            rank(a.kind)
                .cmp(&rank(b.kind))
                .then_with(|| natord::compare(&b.path.to_string_lossy(), &a.path.to_string_lossy()))
        });
        entries.extend(found);
    }
    Ok(entries)
}

/// Make the archived `entry` of artist folder `dir` current again. The file it
/// replaces (if any) is skipped, then archived; the restored contents lose
/// their skip marker, so no run replaces them for being skipped.
pub fn restore(dir: &Path, entry: &Entry, revisions: &Revisions) -> Result<Restored> {
    let kind = entry.kind;
    let digest = revision::file_digest(&entry.path)?;
    let target_dir = kind.target_dir(dir);

    let mut replaced = None;
    if let Some(current) = kind.existing(dir) {
        let current_digest = revision::file_digest(&current)?;
        if current_digest == digest {
            bail!(
                "{} is already the current {}",
                entry.path.display(),
                current.display()
            );
        }
        let relative = archive::skip_and_archive(dir, kind, &current, revisions)?;
        replaced = Some((current, relative));
    }

    fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating {}", target_dir.display()))?;
    let ext = entry
        .path
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let current = target_dir.join(format!("{}{ext}", kind.stem()));
    revision::move_file(&entry.path, &current, &digest)?;

    let marker = target_dir.join(skip::marker_name(&digest));
    if marker.is_file() {
        fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
    }
    Ok(Restored { current, replaced })
}

/// What `gatefold restore` was pointed at.
pub enum Target<'a> {
    /// `--dir`: an artist folder, whose archive is listed to choose from.
    Dir(&'a Path),
    /// `--file`: an archived file, restored directly.
    File(&'a Path),
}

/// `gatefold restore --dir <artist>` lists the folder's archive and asks which
/// one to restore; `gatefold restore --file <archived file>` restores it directly.
/// With `keep`, the restored file also gets its keep marker. Returns the artist
/// folder when a file was restored.
pub fn run(target: &Target, keep: bool) -> Result<Option<PathBuf>> {
    let revisions = Revisions::new();
    let (dir, entry) = match *target {
        Target::File(path) => {
            if !path.is_file() {
                bail!("{} is not a file", path.display());
            }
            let kind = kind_of(path).context(format!(
            "{} is not an archived artist file (expected <artist>/{ARCHIVE_DIR}/<primary|backdrops|logos>/<file>)",
            path.display()
        ))?;
            let dir = artist_dir_of(path)
                .context(format!("{} is not inside {ARCHIVE_DIR}/", path.display()))?
                .to_path_buf();
            (
                dir,
                Entry {
                    kind,
                    path: path.to_path_buf(),
                },
            )
        }
        Target::Dir(path) => {
            let entries = list(path)?;
            if entries.is_empty() {
                println!("nothing archived in {}", path.join(ARCHIVE_DIR).display());
                return Ok(None);
            }
            let stdin = std::io::stdin();
            let Some(entry) = choose(path, &entries, &mut stdin.lock(), &mut std::io::stdout())?
            else {
                println!("nothing restored");
                return Ok(None);
            };
            (path.to_path_buf(), entry)
        }
    };

    let restored = restore(&dir, &entry, &revisions)?;
    let name = |p: &Path| p.strip_prefix(&dir).unwrap_or(p).display().to_string();
    println!(
        "✓ restored {} as {}",
        entry.path.file_name().unwrap_or_default().to_string_lossy(),
        name(&restored.current)
    );
    if let Some((previous, archived)) = &restored.replaced {
        println!("  {} skipped and archived as {archived}", name(previous));
    }
    if keep {
        let marker = keep::mark_artist_image(&dir, entry.kind)?;
        println!("  kept ({})", name(marker.path()));
    } else if !keep::artist_image(&dir, entry.kind) {
        println!("  run with --keep to stop --replace-artist-images from replacing it");
    }
    Ok(Some(dir))
}

/// Print the numbered archive of artist folder `dir` and read a choice. `None`
/// on an empty answer or end of input.
fn choose(
    dir: &Path,
    entries: &[Entry],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<Entry>> {
    writeln!(output, "Archived artist files in {}:", dir.display())?;
    let mut last = None;
    for (i, entry) in entries.iter().enumerate() {
        let kind = entry.kind;
        if last != Some(kind) {
            let current = kind.existing(dir).map_or_else(
                || "none".to_string(),
                |p| p.strip_prefix(dir).unwrap_or(&p).display().to_string(),
            );
            writeln!(output, "\n  {} (current: {current})", label(kind))?;
            last = Some(kind);
        }
        let size = fs::metadata(&entry.path).map_or(0, |m| m.len());
        let skipped = revision::file_digest(&entry.path).is_ok_and(|digest| {
            kind.target_dir(dir)
                .join(skip::marker_name(&digest))
                .is_file()
        });
        writeln!(
            output,
            "  {:>3}) {}  {}{}",
            i + 1,
            entry.path.file_name().unwrap_or_default().to_string_lossy(),
            human_size(size),
            if skipped { "  (skipped)" } else { "" }
        )?;
    }
    loop {
        write!(
            output,
            "\nRestore which? [1-{}, empty to cancel]: ",
            entries.len()
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(None);
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=entries.len()).contains(&n) => return Ok(Some(entries[n - 1].clone())),
            _ => writeln!(output, "  `{answer}` isn't one of the numbers above")?,
        }
    }
}

fn label(kind: ArtistImage) -> &'static str {
    match kind {
        ArtistImage::Avatar => "Avatar",
        ArtistImage::Backdrop => "Backdrop",
        ArtistImage::Video => "Motion backdrop",
        ArtistImage::Logo => "Logo",
    }
}

#[allow(clippy::cast_precision_loss)] // display only
fn human_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::digest_bytes;

    fn artist(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gatefold-restore-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(ARCHIVE_DIR).join("primary")).unwrap();
        fs::create_dir_all(dir.join(ARCHIVE_DIR).join("backdrops")).unwrap();
        dir
    }

    #[test]
    fn lists_archived_versions_by_kind_newest_first() {
        let dir = artist("list");
        let archive = dir.join(ARCHIVE_DIR);
        for name in [
            "primary/folder-2026-01-31.jpg",
            "primary/folder-2026-03-01.jpg",
            "primary/folder-2026-03-01.jpg.sha256",
            "primary/notes.txt",
            "backdrops/backdrop-2026-02-01.mp4",
            "backdrops/backdrop-2026-02-01.jpg",
        ] {
            fs::write(archive.join(name), b"x").unwrap();
        }
        let names: Vec<_> = list(&dir)
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.kind,
                    e.path.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (ArtistImage::Avatar, "folder-2026-03-01.jpg".to_string()),
                (ArtistImage::Avatar, "folder-2026-01-31.jpg".to_string()),
                (ArtistImage::Backdrop, "backdrop-2026-02-01.jpg".to_string()),
                (ArtistImage::Video, "backdrop-2026-02-01.mp4".to_string()),
            ]
        );
        assert!(list(&dir.join("nope")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restoring_skips_and_archives_the_current_file() {
        let dir = artist("restore");
        let revisions = Revisions::new();
        let date = revisions.date();
        let old = dir.join(ARCHIVE_DIR).join("primary/folder-2026-01-31.jpg");
        fs::write(&old, b"old").unwrap();
        // The old photo had been skipped once: restoring it takes that back.
        fs::write(dir.join(skip::marker_name(&digest_bytes(b"old"))), b"").unwrap();
        fs::write(dir.join("folder.jpg"), b"new").unwrap();

        let entry = Entry {
            kind: ArtistImage::Avatar,
            path: old.clone(),
        };
        let restored = restore(&dir, &entry, &revisions).unwrap();
        let archived = format!("{ARCHIVE_DIR}/primary/folder-{date}.jpg");
        assert_eq!(restored.current, dir.join("folder.jpg"));
        assert_eq!(
            restored.replaced,
            Some((dir.join("folder.jpg"), archived.clone()))
        );

        assert_eq!(fs::read(dir.join("folder.jpg")).unwrap(), b"old");
        assert_eq!(
            fs::read_to_string(dir.join("folder.jpg.sha256"))
                .unwrap()
                .trim(),
            digest_bytes(b"old")
        );
        assert!(!old.exists());
        assert_eq!(fs::read(dir.join(&archived)).unwrap(), b"new");
        assert!(skip::rejects(&dir, b"new"), "the replaced file is skipped");
        assert!(!skip::rejects(&dir, b"old"), "the restored file isn't");

        // Restoring what's already current is refused.
        let again = dir.join(&archived);
        fs::write(dir.join("folder.jpg"), b"new").unwrap();
        fs::remove_file(dir.join("folder.jpg.sha256")).unwrap();
        let entry = Entry {
            kind: ArtistImage::Avatar,
            path: again,
        };
        assert!(restore(&dir, &entry, &revisions).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restores_a_motion_backdrop_into_backdrops() {
        let dir = artist("video");
        let old = dir
            .join(ARCHIVE_DIR)
            .join("backdrops/backdrop-2026-02-01.mp4");
        fs::write(&old, b"old video").unwrap();
        assert_eq!(kind_of(&old), Some(ArtistImage::Video));
        assert_eq!(artist_dir_of(&old), Some(dir.as_path()));

        let entry = Entry {
            kind: ArtistImage::Video,
            path: old,
        };
        let restored = restore(&dir, &entry, &Revisions::new()).unwrap();
        assert_eq!(restored.current, dir.join("backdrops/backdrop.mp4"));
        assert_eq!(restored.replaced, None);
        assert_eq!(fs::read(&restored.current).unwrap(), b"old video");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn choosing_reads_a_number_and_retries_on_nonsense() {
        let dir = artist("choose");
        let archive = dir.join(ARCHIVE_DIR);
        fs::write(archive.join("primary/folder-2026-01-31.jpg"), b"a").unwrap();
        fs::write(archive.join("backdrops/backdrop-2026-02-01.jpg"), b"b").unwrap();
        let entries = list(&dir).unwrap();

        let mut out = Vec::new();
        let picked = choose(&dir, &entries, &mut "9\nx\n2\n".as_bytes(), &mut out).unwrap();
        assert_eq!(picked.unwrap().kind, ArtistImage::Backdrop);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Avatar (current: none)"), "{out}");
        assert!(out.contains("`9` isn't one of the numbers above"), "{out}");

        let mut sink = Vec::new();
        assert!(
            choose(&dir, &entries, &mut "\n".as_bytes(), &mut sink)
                .unwrap()
                .is_none()
        );
        assert!(
            choose(&dir, &entries, &mut "".as_bytes(), &mut sink)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
