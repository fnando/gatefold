use crate::backup::Backup;
use crate::revision::{self, Archived, Revisions};
use crate::state::{CachedArt, digest_bytes};
use crate::{scanner, verify};
use anyhow::{Context, Result, bail};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::read_from_path;
use lofty::tag::{ItemKey, Tag};
use std::fs;
use std::path::{Path, PathBuf};

/// Cover-art sidecar filenames we treat as "already present" in an album folder.
/// `folder.jpg` is also the artist image name, which is why artist images are only
/// ever written to folders that hold no audio (see `artist::find_artist_folder`).
const COVER_FILENAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.png",
];

/// Hidden folder in each artist folder holding replaced artist files. Jellyfin
/// ignores dot-prefixed paths (`**/.*` in its built-in ignore list), so archived
/// images and videos are never indexed or played as theme videos.
pub const ARCHIVE_DIR: &str = ".gatefold-archive";

/// Where archives lived before [`ARCHIVE_DIR`] (`primary/`, `backdrops/`,
/// `logos/` in the artist folder itself), with the stem of the files archived
/// there and their subfolder under [`ARCHIVE_DIR`] now.
const LEGACY_ARCHIVES: [(&str, &str); 3] = [
    ("primary", "folder"),
    ("backdrops", "backdrop"),
    ("logos", "logo"),
];

/// The artist files written into an artist folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtistImage {
    /// Square primary image, `folder.jpg`.
    Avatar,
    /// Wide background, `backdrop.jpg`.
    Backdrop,
    /// Motion background, `backdrops/backdrop.mp4` (Jellyfin's theme-video folder,
    /// which keeps the video out of the music scan).
    Video,
    /// Transparent wordmark, `logo.png`.
    Logo,
}

impl ArtistImage {
    pub fn stem(self) -> &'static str {
        match self {
            ArtistImage::Avatar => "folder",
            ArtistImage::Backdrop | ArtistImage::Video => "backdrop",
            ArtistImage::Logo => "logo",
        }
    }

    /// Filenames that count as this file already being present.
    pub fn names(self) -> &'static [&'static str] {
        match self {
            ArtistImage::Avatar => &["folder.jpg", "folder.jpeg", "folder.png"],
            ArtistImage::Backdrop => &["backdrop.jpg", "backdrop.jpeg", "backdrop.png"],
            ArtistImage::Video => &[
                "backdrop.mp4",
                "backdrop.m4v",
                "backdrop.mov",
                "backdrop.webm",
            ],
            ArtistImage::Logo => &["logo.png", "logo.jpg", "logo.jpeg"],
        }
    }

    /// Subfolder of the artist folder this file lives in, if not the folder itself.
    fn subdir(self) -> Option<&'static str> {
        match self {
            ArtistImage::Avatar | ArtistImage::Backdrop | ArtistImage::Logo => None,
            ArtistImage::Video => Some("backdrops"),
        }
    }

    /// Folder of the artist folder `dir` that keeps replaced versions of this
    /// file: `.gatefold-archive/primary`, `…/backdrops` or `…/logos`.
    pub fn archive_dir(self, dir: &Path) -> PathBuf {
        let sub = match self {
            ArtistImage::Avatar => "primary",
            ArtistImage::Backdrop | ArtistImage::Video => "backdrops",
            ArtistImage::Logo => "logos",
        };
        dir.join(ARCHIVE_DIR).join(sub)
    }

    /// Where this file goes, relative to the artist folder (for logs).
    pub fn label(self, ext: &str) -> String {
        match self.subdir() {
            Some(sub) => format!("{sub}/{}.{ext}", self.stem()),
            None => format!("{}.{ext}", self.stem()),
        }
    }

    /// The folder this file is written into, under the artist folder `dir`.
    pub fn target_dir(self, dir: &Path) -> PathBuf {
        self.subdir()
            .map_or_else(|| dir.to_path_buf(), |sub| dir.join(sub))
    }

    /// The existing file for this image under artist folder `dir`, if any.
    pub fn existing(self, dir: &Path) -> Option<PathBuf> {
        let target = self.target_dir(dir);
        self.names()
            .iter()
            .map(|name| target.join(name))
            .find(|path| path.is_file())
    }
}

/// The result of attempting to write a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// Left untouched (already present and not replacing, or identical value).
    Skipped,
    /// Written where nothing was present before.
    Created,
    /// An existing, differing value was overwritten (its previous value backed up).
    Replaced,
}

impl Write {
    pub fn happened(self) -> bool {
        !matches!(self, Write::Skipped)
    }
}

/// True when a lyrics value (synced or unsynced) is already present and non-empty.
pub fn tag_has_lyrics(tag: &Tag) -> bool {
    [ItemKey::UnsyncLyrics, ItemKey::Lyrics]
        .into_iter()
        .filter_map(|key| tag.get_string(key))
        .any(|s| !s.trim().is_empty())
}

/// True when the tag already embeds a front-cover picture.
pub fn tag_has_front_cover(tag: &Tag) -> bool {
    tag.pictures()
        .iter()
        .any(|p| p.pic_type() == PictureType::CoverFront)
}

/// True when the folder already contains a recognized cover-art image.
pub fn folder_has_cover(dir: &Path) -> bool {
    COVER_FILENAMES.iter().any(|name| dir.join(name).is_file())
}

/// Write `<stem>.lrc` next to the track. Never overwrites an existing file.
/// Returns true when a file was created.
pub fn write_lrc(path: &Path, synced: &str) -> Result<bool> {
    let lrc = scanner::lrc_path_for(path);
    if lrc.exists() {
        return Ok(false);
    }
    fs::write(&lrc, synced).with_context(|| format!("writing {}", lrc.display()))?;
    Ok(true)
}

/// Embed `lyrics` into the file's unsynced-lyrics tag. When an existing value is
/// present it's kept unless `replace` is set and the value differs, in which case
/// the previous value is backed up. Returns what happened.
pub fn write_lyrics_tag(
    path: &Path,
    lyrics: &str,
    replace: bool,
    verify: bool,
    backup: Option<&Backup>,
) -> Result<Write> {
    let mut tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;

    if tagged.primary_tag().is_none() {
        let tag_type = tagged.file_type().primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }

    let tag = tagged
        .primary_tag_mut()
        .expect("primary tag just ensured to exist");

    let existing = tag
        .get_string(ItemKey::UnsyncLyrics)
        .or_else(|| tag.get_string(ItemKey::Lyrics))
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());

    if let Some(prev) = &existing
        && (!replace || prev == lyrics)
    {
        return Ok(Write::Skipped);
    }
    let replaced = existing.is_some();

    // `ItemKey::Lyrics` is unsupported in ID3v2; `UnsyncLyrics` maps to USLT /
    // Vorbis UNSYNCEDLYRICS / MP4 ©lyr and is what we want for plain lyrics.
    if !tag.insert_text(ItemKey::UnsyncLyrics, lyrics.to_string()) {
        bail!("this format does not support an unsynced-lyrics tag");
    }
    save_tag(tag, path, verify)?;

    if let (Some(prev), Some(backup)) = (&existing, backup) {
        backup.snapshot("lyrics.txt", prev.as_bytes())?;
    }
    Ok(if replaced {
        Write::Replaced
    } else {
        Write::Created
    })
}

/// Embed `art` as the front-cover picture. An existing front cover is kept unless
/// `replace` is set and the image differs (as cached or as downloaded), in which
/// case the previous image is backed up. Returns what happened.
pub fn embed_cover(
    path: &Path,
    art: &CachedArt,
    replace: bool,
    verify: bool,
    backup: Option<&Backup>,
) -> Result<Write> {
    let mut tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;

    if tagged.primary_tag().is_none() {
        let tag_type = tagged.file_type().primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }

    let tag = tagged
        .primary_tag_mut()
        .expect("primary tag just ensured to exist");

    // Capture the previous cover (owned) while briefly borrowing it.
    let previous = match tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
    {
        None => None,
        Some(_) if !replace => return Ok(Write::Skipped),
        Some(old) if art.is(&digest_bytes(old.data())) => return Ok(Write::Skipped),
        Some(old) => Some((
            format!("cover.{}", extension_for(old.mime_type())),
            old.data().to_vec(),
        )),
    };
    let replaced = previous.is_some();

    if replaced {
        tag.remove_picture_type(PictureType::CoverFront);
    }
    let picture = Picture::unchecked(art.data.clone())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::from_str(&art.mime))
        .build();
    tag.push_picture(picture);
    save_tag(tag, path, verify)?;

    if let (Some((name, bytes)), Some(backup)) = (&previous, backup) {
        backup.snapshot(name, bytes)?;
    }
    Ok(if replaced {
        Write::Replaced
    } else {
        Write::Created
    })
}

/// Pass the front cover embedded in the track at `path` through `optimize`;
/// when that returns smaller bytes, put them in its place (same type, MIME and
/// description) and save the tag (verified with ffprobe under `verify`).
/// Returns the cover's size before and after, or `None` when it has no front
/// cover or `optimize` had nothing smaller. Under `dry_run` nothing is saved.
pub fn optimize_embedded_cover(
    path: &Path,
    verify: bool,
    dry_run: bool,
    optimize: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Result<Option<(usize, usize)>> {
    let mut tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;
    let Some(tag) = tagged.primary_tag_mut() else {
        return Ok(None);
    };
    let Some(index) = tag
        .pictures()
        .iter()
        .position(|p| p.pic_type() == PictureType::CoverFront)
    else {
        return Ok(None);
    };
    let old = &tag.pictures()[index];
    let Some(data) = optimize(old.data()) else {
        return Ok(None);
    };
    let sizes = (old.data().len(), data.len());
    if dry_run {
        return Ok(Some(sizes));
    }
    let mut picture = Picture::unchecked(data).pic_type(PictureType::CoverFront);
    if let Some(mime) = old.mime_type() {
        picture = picture.mime_type(mime.clone());
    }
    if let Some(description) = old.description() {
        picture = picture.description(description.to_string());
    }
    tag.set_picture(index, picture.build());
    save_tag(tag, path, verify)?;
    Ok(Some(sizes))
}

/// Persist `tag` into the file at `path`. With `verify`, the change is applied to a
/// sibling temp copy, validated with ffprobe, then atomically renamed over the
/// original — so a failed check never leaves a corrupted file.
fn save_tag(tag: &Tag, path: &Path, verify: bool) -> Result<()> {
    if !verify {
        return tag
            .save_to_path(path, WriteOptions::default())
            .with_context(|| format!("saving tags to {}", path.display()));
    }

    let work = temp_sibling(path);
    let applied = (|| -> Result<()> {
        fs::copy(path, &work)
            .with_context(|| format!("copying {} for verification", path.display()))?;
        tag.save_to_path(&work, WriteOptions::default())
            .with_context(|| format!("saving tags to {}", work.display()))?;
        verify::audio(&work)
    })();

    match applied {
        Ok(()) => fs::rename(&work, path).with_context(|| format!("replacing {}", path.display())),
        Err(err) => {
            let _ = fs::remove_file(&work);
            Err(err)
        }
    }
}

/// A temp path alongside `path` (same directory, so the rename stays on one filesystem).
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".gatefold-tmp");
    PathBuf::from(name)
}

/// The `cover.*` file gatefold manages in a folder, if present. `folder.jpg` and
/// friends count as "a cover exists" (see [`folder_has_cover`]) but are never
/// written or replaced by us.
fn existing_cover_sidecar(dir: &Path) -> Option<PathBuf> {
    ["cover.jpg", "cover.jpeg", "cover.png"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// Write `cover.<ext>` into the track's folder. An existing `cover.*` is kept
/// unless `replace` is set and the bytes differ, in which case the previous file
/// is snapshotted to the backup (as `cover-sidecar.<ext>`) and swapped out. A
/// folder whose cover is some other file (`folder.jpg`) is left alone.
pub fn write_cover_sidecar(
    dir: &Path,
    art: &CachedArt,
    replace: bool,
    backup: Option<&Backup>,
) -> Result<Write> {
    let previous = match existing_cover_sidecar(dir) {
        None if folder_has_cover(dir) => return Ok(Write::Skipped),
        None => None,
        Some(_) if !replace => return Ok(Write::Skipped),
        Some(path) => {
            let current = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if art.is(&digest_bytes(&current)) {
                return Ok(Write::Skipped);
            }
            Some((path, current))
        }
    };

    if let Some((path, bytes)) = &previous {
        if let Some(backup) = backup {
            let old_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            backup.snapshot(&format!("cover-sidecar.{old_ext}"), bytes)?;
        }
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }

    let path = dir.join(format!("cover.{}", art.ext));
    let work = temp_sibling(&path);
    fs::write(&work, &art.data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(if previous.is_some() {
        Write::Replaced
    } else {
        Write::Created
    })
}

/// Move archives left by earlier versions in `primary/`, `backdrops/` and
/// `logos/` of artist folder `dir` (where Jellyfin indexes them, and plays old
/// motion backdrops as theme videos) into [`ARCHIVE_DIR`]. Only dated archive
/// files (`folder-2026-01-31.jpg`, `backdrop-2026-01-31-2.mp4`, …) and their
/// digest sidecars move; the current `backdrops/backdrop.mp4` and anything else
/// stay, and a legacy folder is removed once empty. Returns how many files moved.
pub fn migrate_archives(dir: &Path) -> Result<usize> {
    let mut moved = 0;
    for (sub, stem) in LEGACY_ARCHIVES {
        let legacy = dir.join(sub);
        let Ok(entries) = fs::read_dir(&legacy) else {
            continue;
        };
        let target = dir.join(ARCHIVE_DIR).join(sub);
        for entry in entries {
            let path = entry
                .with_context(|| format!("reading {}", legacy.display()))?
                .path();
            if path.is_file() && revision::is_archived_name(&path, stem) {
                revision::relocate(&path, &target)?;
                moved += 1;
            }
        }
        // Only succeeds when nothing else is left in it.
        let _ = fs::remove_dir(&legacy);
    }
    Ok(moved)
}

/// Write an artist image into `dir` as `<stem>.<ext>`, with a `<file>.sha256`
/// digest sidecar. An existing image is kept unless `replace` is set and its
/// digest (read from its sidecar, so large files aren't reread) is neither the
/// image's as cached nor as downloaded, in which
/// case the previous file is moved into its dated archive folder first. Returns
/// what happened and, when a file was moved aside, where it went (relative to `dir`).
pub fn write_artist_image(
    dir: &Path,
    kind: ArtistImage,
    art: &CachedArt,
    replace: bool,
    revisions: &Revisions,
) -> Result<(Write, Option<String>)> {
    let digest = digest_bytes(&art.data);
    let existing = match kind.existing(dir) {
        None => None,
        Some(_) if !replace => return Ok((Write::Skipped, None)),
        Some(path) => {
            let current = revision::file_digest(&path)?;
            if art.is(&current) {
                return Ok((Write::Skipped, None));
            }
            Some((path, current))
        }
    };

    let mut kept = None;
    if let Some((previous, previous_digest)) = &existing {
        let archived = revisions.archive(&kind.archive_dir(dir), previous, previous_digest)?;
        let relative = archived
            .path()
            .strip_prefix(dir)
            .unwrap_or(archived.path())
            .to_string_lossy()
            .into_owned();
        kept = Some(match archived {
            Archived::Moved(_) => relative,
            Archived::Duplicate(_) => format!("{relative} (already kept)"),
        });
    }

    let target = kind.target_dir(dir);
    if kind.subdir().is_some() {
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    }
    let path = target.join(format!("{}.{}", kind.stem(), art.ext));
    let work = temp_sibling(&path);
    fs::write(&work, &art.data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, &path).with_context(|| format!("replacing {}", path.display()))?;
    revision::write_digest(&path, &digest)?;

    let write = if existing.is_some() {
        Write::Replaced
    } else {
        Write::Created
    };
    Ok((write, kept))
}

/// Archive `data`, an artist image gatefold won't write into `dir` (kept or
/// skipped), as an alternative `gatefold restore` can bring in: dated in the
/// kind's archive folder, unless it's the current file or already archived.
/// Returns where it went, relative to `dir`.
pub fn archive_alternative(
    dir: &Path,
    kind: ArtistImage,
    art: &CachedArt,
    revisions: &Revisions,
) -> Result<Option<String>> {
    if let Some(current) = kind.existing(dir)
        && art.is(&revision::file_digest(&current)?)
    {
        return Ok(None);
    }
    let archive = kind.archive_dir(dir);
    let saved = revisions.archive_bytes(&archive, kind.stem(), &art.ext, &art.data)?;
    Ok(saved.map(|path| {
        path.strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned()
    }))
}

/// File extension for a picture's MIME type, defaulting to `jpg`.
fn extension_for(mime: Option<&MimeType>) -> &'static str {
    match mime.map(MimeType::as_str) {
        Some("image/png") => "png",
        Some("image/gif") => "gif",
        _ => "jpg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gatefold-writer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaced_artist_files_go_to_the_hidden_archive() {
        let dir = artist("archive");
        let revisions = Revisions::new();
        let date = revisions.date();
        for (kind, ext, sub) in [
            (ArtistImage::Avatar, "jpg", "primary"),
            (ArtistImage::Backdrop, "jpg", "backdrops"),
            (ArtistImage::Video, "mp4", "backdrops"),
            (ArtistImage::Logo, "png", "logos"),
        ] {
            let old = format!("old {sub} {ext}");
            let old_art = CachedArt::new(ext, old.as_bytes());
            write_artist_image(&dir, kind, &old_art, false, &revisions).unwrap();
            let new_art = CachedArt::new(ext, b"new");
            let (wrote, kept) = write_artist_image(&dir, kind, &new_art, true, &revisions).unwrap();
            assert_eq!(wrote, Write::Replaced);
            let stem = kind.stem();
            let archived = format!("{ARCHIVE_DIR}/{sub}/{stem}-{date}.{ext}");
            assert_eq!(kept.as_deref(), Some(archived.as_str()));
            assert_eq!(fs::read(dir.join(&archived)).unwrap(), old.as_bytes());
        }
        for legacy in ["primary", "logos"] {
            assert!(!dir.join(legacy).exists(), "{legacy}/ is no longer used");
        }
        // Only the current motion backdrop is in Jellyfin's theme-video folder.
        let theme: Vec<_> = fs::read_dir(dir.join("backdrops"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.ends_with(".sha256"))
            .collect();
        assert_eq!(theme, ["backdrop.mp4"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_with_the_downloaded_bytes_is_the_optimized_image() {
        let dir = artist("original");
        let revisions = Revisions::new();
        write_artist_image(
            &dir,
            ArtistImage::Avatar,
            &CachedArt::new("jpg", b"as downloaded"),
            false,
            &revisions,
        )
        .unwrap();
        let mut optimized = CachedArt::new("jpg", b"optimized");
        optimized.original = Some(digest_bytes(b"as downloaded"));
        let (wrote, kept) =
            write_artist_image(&dir, ArtistImage::Avatar, &optimized, true, &revisions).unwrap();
        assert_eq!((wrote, kept), (Write::Skipped, None));
        assert_eq!(fs::read(dir.join("folder.jpg")).unwrap(), b"as downloaded");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_archives_move_into_the_hidden_archive() {
        let dir = artist("migrate");
        for sub in ["primary", "backdrops", "logos"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("primary/folder-2026-01-31.jpg"), b"a1").unwrap();
        fs::write(dir.join("primary/folder-2026-01-31.jpg.sha256"), b"stale\n").unwrap();
        fs::write(dir.join("primary/folder-2026-01-31-2.jpg"), b"a2").unwrap();
        fs::write(dir.join("backdrops/backdrop.mp4"), b"current video").unwrap();
        fs::write(dir.join("backdrops/backdrop-2026-02-01.mp4"), b"v1").unwrap();
        fs::write(dir.join("backdrops/backdrop-2026-02-01.jpg"), b"b1").unwrap();
        fs::write(dir.join("backdrops/clip.mp4"), b"user's own").unwrap();
        fs::write(dir.join("logos/logo-2026-03-01.png"), b"l1").unwrap();
        // Already archived under the new layout: the legacy duplicate is dropped.
        let primary = dir.join(ARCHIVE_DIR).join("primary");
        fs::create_dir_all(&primary).unwrap();
        fs::write(primary.join("folder-2025-12-01.jpg"), b"a2").unwrap();

        assert_eq!(migrate_archives(&dir).unwrap(), 5);
        assert_eq!(
            migrate_archives(&dir).unwrap(),
            0,
            "a second run is a no-op"
        );

        let archive = dir.join(ARCHIVE_DIR);
        assert_eq!(
            fs::read(archive.join("primary/folder-2026-01-31.jpg")).unwrap(),
            b"a1"
        );
        assert_eq!(
            fs::read_to_string(archive.join("primary/folder-2026-01-31.jpg.sha256"))
                .unwrap()
                .trim(),
            crate::state::digest_bytes(b"a1"),
            "a malformed sidecar is recomputed"
        );
        assert!(!archive.join("primary/folder-2026-01-31-2.jpg").exists());
        assert!(archive.join("backdrops/backdrop-2026-02-01.mp4").is_file());
        assert!(archive.join("backdrops/backdrop-2026-02-01.jpg").is_file());
        assert!(archive.join("logos/logo-2026-03-01.png").is_file());

        assert!(!dir.join("primary").exists());
        assert!(!dir.join("logos").exists());
        assert!(dir.join("backdrops/backdrop.mp4").is_file());
        assert!(dir.join("backdrops/clip.mp4").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
}
