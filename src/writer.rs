use crate::backup::Backup;
use crate::revision::{self, Archived, Revisions};
use crate::state::digest_bytes;
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

    /// Subfolder of the artist folder that keeps replaced versions of this file.
    fn archive_dir(self) -> &'static str {
        match self {
            ArtistImage::Avatar => "primary",
            ArtistImage::Backdrop | ArtistImage::Video => "backdrops",
            ArtistImage::Logo => "logos",
        }
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

/// Embed `data` as the front-cover picture. An existing front cover is kept unless
/// `replace` is set and the image differs, in which case the previous image is
/// backed up. Returns what happened.
pub fn embed_cover(
    path: &Path,
    mime: &str,
    data: &[u8],
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
        Some(old) if old.data() == data => return Ok(Write::Skipped),
        Some(old) => Some((
            format!("cover.{}", extension_for(old.mime_type())),
            old.data().to_vec(),
        )),
    };
    let replaced = previous.is_some();

    if replaced {
        tag.remove_picture_type(PictureType::CoverFront);
    }
    let picture = Picture::unchecked(data.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::from_str(mime))
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
    ext: &str,
    data: &[u8],
    replace: bool,
    backup: Option<&Backup>,
) -> Result<Write> {
    let previous = match existing_cover_sidecar(dir) {
        None if folder_has_cover(dir) => return Ok(Write::Skipped),
        None => None,
        Some(_) if !replace => return Ok(Write::Skipped),
        Some(path) => {
            let current = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if current == data {
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

    let path = dir.join(format!("cover.{ext}"));
    let work = temp_sibling(&path);
    fs::write(&work, data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(if previous.is_some() {
        Write::Replaced
    } else {
        Write::Created
    })
}

/// Write an artist image into `dir` as `<stem>.<ext>`, with a `<file>.sha256`
/// digest sidecar. An existing image is kept unless `replace` is set and its
/// digest differs (read from its sidecar, so large files aren't reread), in which
/// case the previous file is moved into its dated archive folder first. Returns
/// what happened and, when a file was moved aside, where it went (relative to `dir`).
pub fn write_artist_image(
    dir: &Path,
    kind: ArtistImage,
    ext: &str,
    data: &[u8],
    replace: bool,
    revisions: &Revisions,
) -> Result<(Write, Option<String>)> {
    let digest = digest_bytes(data);
    let existing = match kind.existing(dir) {
        None => None,
        Some(_) if !replace => return Ok((Write::Skipped, None)),
        Some(path) => {
            let current = revision::file_digest(&path)?;
            if current == digest {
                return Ok((Write::Skipped, None));
            }
            Some((path, current))
        }
    };

    let mut kept = None;
    if let Some((previous, previous_digest)) = &existing {
        let archived =
            revisions.archive(&dir.join(kind.archive_dir()), previous, previous_digest)?;
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
    let path = target.join(format!("{}.{ext}", kind.stem()));
    let work = temp_sibling(&path);
    fs::write(&work, data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, &path).with_context(|| format!("replacing {}", path.display()))?;
    revision::write_digest(&path, &digest)?;

    let write = if existing.is_some() {
        Write::Replaced
    } else {
        Write::Created
    };
    Ok((write, kept))
}

/// File extension for a picture's MIME type, defaulting to `jpg`.
fn extension_for(mime: Option<&MimeType>) -> &'static str {
    match mime.map(MimeType::as_str) {
        Some("image/png") => "png",
        Some("image/gif") => "gif",
        _ => "jpg",
    }
}
