use crate::backup::Backup;
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
const COVER_FILENAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.png",
];

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

/// Write `cover.<ext>` into the track's folder. Never overwrites an existing
/// cover image. Returns true when a file was created.
pub fn write_cover_sidecar(dir: &Path, ext: &str, data: &[u8]) -> Result<bool> {
    if folder_has_cover(dir) {
        return Ok(false);
    }
    let path = dir.join(format!("cover.{ext}"));
    fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// File extension for a picture's MIME type, defaulting to `jpg`.
fn extension_for(mime: Option<&MimeType>) -> &'static str {
    match mime.map(MimeType::as_str) {
        Some("image/png") => "png",
        Some("image/gif") => "gif",
        _ => "jpg",
    }
}
