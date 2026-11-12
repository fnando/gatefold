//! `gatefold optimize --dir <folder>`: shrink the images and motion backdrops
//! already in a library, as new downloads are. Walks each folder for the files
//! gatefold manages: artist files (`folder.*`, `backdrop.*`, `logo.*`,
//! `backdrops/backdrop.mp4`) and their `.gatefold-archive/`, album covers
//! (`cover.*`) and the front covers embedded in tracks.
//!
//! - Images are optimized losslessly (see `lossless`): pixels never change.
//! - Motion backdrops still as Apple served them get the re-encode every
//!   download gets (`video::shrink`: 1080p x264). Ones already re-encoded, and
//!   HDR ones (which downloads keep as they are), are left alone, so a second
//!   run never re-encodes anything again.
//! - Kept files (`gatefold keep`) are left byte for byte as they are.
//! - A skipped image stays skipped: its marker is renamed after the new
//!   contents. Digest sidecars are refreshed.
//! - The same cover embedded in every track of an album is optimized once.

use crate::revision;
use crate::skip::{self, Managed};
use crate::state::digest_bytes;
use crate::video::{self, Existing};
use crate::writer::{self, ARCHIVE_DIR, ArtistImage};
use crate::{keep, lossless, scanner};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png"];
/// What motion backdrops are saved as (see `ArtistImage::Video`).
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "webm"];

pub struct Options {
    pub dry_run: bool,
    pub verify: bool,
}

/// What to do with one file found under a folder.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A current image or motion backdrop; its skip marker (if any) lives in
    /// `marker_dir`.
    Image {
        kept: bool,
        video: bool,
        marker_dir: PathBuf,
    },
    /// An archived artist file; a skip of it lives where the current one is.
    Archived {
        video: bool,
        marker_dir: PathBuf,
    },
    Track {
        kept: bool,
    },
}

/// Per-run tallies, and the optimized bytes by original digest (so a cover
/// embedded in every track of an album is optimized once).
#[derive(Default)]
struct Run {
    images: usize,
    covers: usize,
    videos: usize,
    /// Motion backdrops left as they are: no ffmpeg with libx264.
    no_encoder: usize,
    /// Whether this ffmpeg can re-encode backdrops (checked on the first one).
    can_shrink: Option<bool>,
    kept: usize,
    failed: usize,
    before: u64,
    after: u64,
    memo: HashMap<String, Option<Vec<u8>>>,
}

impl Run {
    fn optimized(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.memo
            .entry(digest_bytes(data))
            .or_insert_with(|| lossless::image(data))
            .clone()
    }

    fn saved(&mut self, (before, after): (usize, usize), what: What) {
        match what {
            What::Image => self.images += 1,
            What::Cover => self.covers += 1,
            What::Video => self.videos += 1,
        }
        self.before += before as u64;
        self.after += after as u64;
    }

    fn can_shrink(&mut self) -> bool {
        *self
            .can_shrink
            .get_or_insert_with(|| video::ffmpeg_available() && video::can_shrink())
    }
}

/// What was shrunk, for the report and the tallies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum What {
    Image,
    Cover,
    Video,
}

impl What {
    fn label(self) -> &'static str {
        match self {
            What::Image => "",
            What::Cover => " (embedded cover)",
            What::Video => " (re-encoded to 1080p)",
        }
    }
}

pub fn run(dirs: &[PathBuf], options: &Options) -> Result<()> {
    let mut run = Run::default();
    for root in dirs {
        if !root.is_dir() {
            bail!("{} is not a folder", root.display());
        }
        let walk = WalkDir::new(root).sort_by_file_name().into_iter();
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    run.failed += 1;
                    eprintln!("✗ {err}");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(target) = target(path, root) else {
                continue;
            };
            if let Err(err) = process(&mut run, path, &target, options) {
                run.failed += 1;
                eprintln!("✗ {}: {err:#}", path.display());
            }
        }
    }
    summarize(&run, options.dry_run);
    if run.failed > 0 {
        bail!("{} file(s) could not be optimized", run.failed);
    }
    Ok(())
}

/// What `path` (under the scanned `root`) is to gatefold, if anything.
fn target(path: &Path, root: &Path) -> Option<Target> {
    let parent = path.parent()?;
    if scanner::is_audio(path) {
        return Some(Target::Track {
            kept: keep::cover(parent, root),
        });
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let video = VIDEO_EXTENSIONS.contains(&ext.as_str());
    if !video && !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    // `<artist>/.gatefold-archive/<primary|backdrops|logos>/<file>`. A skipped
    // motion backdrop's marker is in `<artist>/backdrops/`, next to it.
    let section = parent.parent()?;
    if section.file_name().is_some_and(|n| n == ARCHIVE_DIR) {
        let artist = section.parent()?;
        let marker_dir = if video {
            ArtistImage::Video.target_dir(artist)
        } else {
            artist.to_path_buf()
        };
        return Some(Target::Archived { video, marker_dir });
    }
    let (managed, dir) = skip::classify(path)?;
    let kept = match managed {
        Managed::Cover => keep::cover(&dir, root),
        // `folder.jpg` in a folder with tracks is that album's cover.
        Managed::Artist(_) if scanner::dir_has_audio(&dir) => keep::cover(&dir, root),
        Managed::Artist(kind) => keep::artist_image(&dir, kind),
    };
    Some(Target::Image {
        kept,
        video,
        marker_dir: parent.to_path_buf(),
    })
}

fn process(run: &mut Run, path: &Path, target: &Target, options: &Options) -> Result<()> {
    let marker_dir = match target {
        Target::Track { kept: true } | Target::Image { kept: true, .. } => {
            run.kept += 1;
            return Ok(());
        }
        Target::Track { kept: false } => {
            let sizes =
                writer::optimize_embedded_cover(path, options.verify, options.dry_run, |d| {
                    run.optimized(d)
                })?;
            if let Some(sizes) = sizes {
                report(path, sizes, What::Cover, options.dry_run);
                run.saved(sizes, What::Cover);
            }
            return Ok(());
        }
        Target::Image {
            video: true,
            marker_dir,
            ..
        }
        | Target::Archived {
            video: true,
            marker_dir,
        } => return shrink_video(run, path, marker_dir, options.dry_run),
        Target::Image { marker_dir, .. } | Target::Archived { marker_dir, .. } => marker_dir,
    };
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Some(optimized) = run.optimized(&data) else {
        return Ok(());
    };
    let sizes = (data.len(), optimized.len());
    if !options.dry_run {
        let work = work_path(path);
        fs::write(&work, &optimized).with_context(|| format!("writing {}", work.display()))?;
        swap_in(path, &work, &digest_bytes(&data), marker_dir)?;
    }
    report(path, sizes, What::Image, options.dry_run);
    run.saved(sizes, What::Image);
    Ok(())
}

/// Re-encode a motion backdrop still as Apple served it (see `video::shrink`),
/// keeping the result only when it's smaller. A dry run only names the ones
/// it would re-encode: finding out by how much means encoding them.
fn shrink_video(run: &mut Run, path: &Path, marker_dir: &Path, dry_run: bool) -> Result<()> {
    if !run.can_shrink() {
        run.no_encoder += 1;
        return Ok(());
    }
    if video::inspect(path)? != Existing::Original {
        return Ok(());
    }
    if dry_run {
        println!("~ {}: would be re-encoded to 1080p", path.display());
        run.videos += 1;
        return Ok(());
    }
    let work = work_path(path);
    let sizes = video::shrink(path.as_os_str(), None, &work).and_then(|()| {
        let before = fs::metadata(path)?.len();
        let after = fs::metadata(&work)?.len();
        Ok((before, after))
    });
    let (before, after) = match sizes {
        Ok(sizes) => sizes,
        Err(err) => {
            let _ = fs::remove_file(&work);
            return Err(err);
        }
    };
    if after >= before {
        fs::remove_file(&work).with_context(|| format!("removing {}", work.display()))?;
        return Ok(());
    }
    let old = revision::file_digest(path)?;
    swap_in(path, &work, &old, marker_dir)?;
    let sizes = (usize::try_from(before)?, usize::try_from(after)?);
    report(path, sizes, What::Video, false);
    run.saved(sizes, What::Video);
    Ok(())
}

/// Where a replacement for `path` is written before it's swapped in: hidden,
/// next to it (so the rename stays on one filesystem).
fn work_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.part"))
}

/// Rename `work` over `path` (whose contents had the digest `old`), carrying
/// a skip marker and the digest sidecar over to the new contents.
fn swap_in(path: &Path, work: &Path, old: &str, marker_dir: &Path) -> Result<()> {
    let new = revision::digest_of(work)?;
    fs::rename(work, path).with_context(|| format!("replacing {}", path.display()))?;
    skip::rename(marker_dir, old, &new)?;
    revision::update_digest(path, &new)
}

fn report(path: &Path, (before, after): (usize, usize), what: What, dry_run: bool) {
    let mark = if dry_run { "~" } else { "✓" };
    let what = what.label();
    println!(
        "{mark} {}{what}: {} → {} (−{})",
        path.display(),
        size(before as u64),
        size(after as u64),
        percent(before as u64, after as u64)
    );
}

fn summarize(run: &Run, dry_run: bool) {
    let total = run.images + run.covers + run.videos;
    let verb = if dry_run {
        "Would optimize"
    } else {
        "Optimized"
    };
    let counts = format!(
        "{} image file(s), {} embedded cover(s) and {} motion backdrop(s)",
        run.images, run.covers, run.videos
    );
    let saved = format!(
        "{} → {}, {} saved (−{})",
        size(run.before),
        size(run.after),
        size(run.before - run.after),
        percent(run.before, run.after)
    );
    if total == 0 {
        println!("Nothing to optimize.");
    } else if !dry_run || run.videos == 0 {
        println!("{verb} {counts}: {saved}.");
    } else if run.images + run.covers == 0 {
        // A dry run doesn't encode backdrops, so their savings are unknown.
        println!("{verb} {counts}.");
    } else {
        println!("{verb} {counts}: images {saved}; backdrops aren't measured on a dry run.");
    }
    if run.no_encoder > 0 {
        println!(
            "{} motion backdrop(s) left as they are: re-encoding them needs ffmpeg with libx264.",
            run.no_encoder
        );
    }
    if run.kept > 0 {
        println!("{} kept file(s) and track(s) left as they are.", run.kept);
    }
}

/// `1.2 MB`, `693 KB`, `512 B` (decimal units, like Finder).
fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let value = bytes as f64;
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", value / 1e9)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", value / 1e6)
    } else if bytes >= 1_000 {
        format!("{:.0} KB", value / 1e3)
    } else {
        format!("{bytes} B")
    }
}

fn percent(before: u64, after: u64) -> String {
    if before == 0 {
        return "0%".to_string();
    }
    #[allow(clippy::cast_precision_loss)] // display only
    let saved = (before - after) as f64 / before as f64 * 100.0;
    format!("{saved:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .unwrap()
    }

    fn library(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("gatefold-optimize-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn classifies_what_it_finds() {
        let root = library("classify");
        let artist = root.join("Artist");
        let album = artist.join("2001 - Album");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("01 - Song.flac"), b"").unwrap();
        fs::write(album.join(keep::COVER), b"").unwrap();

        assert_eq!(
            target(&artist.join("folder.jpg"), &root),
            Some(Target::Image {
                kept: false,
                video: false,
                marker_dir: artist.clone()
            })
        );
        assert_eq!(
            target(&album.join("cover.jpg"), &root),
            Some(Target::Image {
                kept: true,
                video: false,
                marker_dir: album.clone()
            })
        );
        assert_eq!(
            target(&album.join("01 - Song.flac"), &root),
            Some(Target::Track { kept: true })
        );
        let archived = ArtistImage::Avatar
            .archive_dir(&artist)
            .join("folder-2026-01-01.jpg");
        assert_eq!(
            target(&archived, &root),
            Some(Target::Archived {
                video: false,
                marker_dir: artist.clone()
            })
        );
        let backdrops = artist.join("backdrops");
        assert_eq!(
            target(&backdrops.join("backdrop.mp4"), &root),
            Some(Target::Image {
                kept: false,
                video: true,
                marker_dir: backdrops.clone()
            })
        );
        let archived_video = ArtistImage::Video
            .archive_dir(&artist)
            .join("backdrop-2026-01-01.mp4");
        assert_eq!(
            target(&archived_video, &root),
            Some(Target::Archived {
                video: true,
                marker_dir: backdrops.clone()
            })
        );
        // Only `backdrops/` holds motion backdrops; tracks are tracks.
        assert_eq!(target(&artist.join("backdrop.mp4"), &root), None);
        assert_eq!(
            target(&album.join("02 - Video.mp4"), &root),
            Some(Target::Track { kept: true })
        );
        assert_eq!(target(&artist.join("notes.jpg"), &root), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn shrinks_files_and_carries_skips_and_digests_over() {
        let root = library("files");
        let artist = root.join("Artist");
        fs::create_dir_all(&artist).unwrap();
        let jpeg = fixture("sample.jpg");
        let png = fixture("sample.png");
        let avatar = artist.join("folder.jpg");
        fs::write(&avatar, &jpeg).unwrap();
        revision::write_digest(&avatar, &digest_bytes(&jpeg)).unwrap();
        skip::mark(&avatar).unwrap();
        fs::write(artist.join("logo.png"), &png).unwrap();
        // A kept backdrop is never touched.
        let backdrop = artist.join("backdrop.jpg");
        fs::write(&backdrop, &jpeg).unwrap();
        fs::write(artist.join(keep::artist_marker(ArtistImage::Backdrop)), b"").unwrap();

        let dry = Options {
            dry_run: true,
            verify: false,
        };
        run(std::slice::from_ref(&root), &dry).unwrap();
        assert_eq!(fs::read(&avatar).unwrap(), jpeg, "a dry run writes nothing");

        let options = Options {
            dry_run: false,
            verify: false,
        };
        run(std::slice::from_ref(&root), &options).unwrap();
        let optimized = fs::read(&avatar).unwrap();
        assert!(optimized.len() < jpeg.len());
        assert!(fs::read(artist.join("logo.png")).unwrap().len() < png.len());
        assert_eq!(fs::read(&backdrop).unwrap(), jpeg, "kept");
        assert!(skip::rejected(&avatar), "still skipped");
        assert!(
            !artist
                .join(skip::marker_name(&digest_bytes(&jpeg)))
                .exists()
        );
        assert_eq!(
            revision::file_digest(&avatar).unwrap(),
            digest_bytes(&optimized)
        );

        // Again: nothing left to gain.
        run(std::slice::from_ref(&root), &options).unwrap();
        assert_eq!(fs::read(&avatar).unwrap(), optimized);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(693_473), "693 KB");
        assert_eq!(size(1_234_567), "1.2 MB");
        assert_eq!(percent(1000, 900), "10.0%");
    }
}
