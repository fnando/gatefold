//! Uploads from the artist and album pages: artist images, covers (written as
//! `cover.*` and embedded in every track), animated covers and lyrics. Each
//! upload says what happens to the file it replaces: keep the upload (so
//! replace runs leave it alone), skip the current one (so gatefold never
//! writes it again), or just replace it. The replaced file is archived, so
//! nothing is lost.

use super::library::{back_to, inside, not_found, record};
use super::{Web, WebError};
use crate::backup::Backup;
use crate::changes::Kind;
use crate::revision::{self, Revisions};
use crate::state::{CachedArt, State as Cache, digest_for};
use crate::writer::{self, ArtistImage};
use crate::{keep, scanner, skip};
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Videos can be large; nothing else comes close.
const MAX_UPLOAD: usize = 200 * 1024 * 1024;

pub fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/actions/upload", post(upload))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
}

/// What to do with the file an upload replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Keep the upload: replace runs leave it alone.
    KeepUpload,
    /// Skip the current file: gatefold never writes it again.
    SkipCurrent,
    /// Just replace it.
    Replace,
}

impl Existing {
    fn parse(value: &str) -> Self {
        match value {
            "skip_current" => Self::SkipCurrent,
            "replace" => Self::Replace,
            _ => Self::KeepUpload,
        }
    }
}

/// What an upload is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Artist(ArtistImage),
    Cover,
    CoverVideo,
    Lyrics,
}

impl Target {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "avatar" => Self::Artist(ArtistImage::Avatar),
            "backdrop" => Self::Artist(ArtistImage::Backdrop),
            "video" => Self::Artist(ArtistImage::Video),
            "logo" => Self::Artist(ArtistImage::Logo),
            "cover" => Self::Cover,
            "cover_video" => Self::CoverVideo,
            "lyrics" => Self::Lyrics,
            _ => return None,
        })
    }
}

#[derive(Default)]
struct Fields {
    target: String,
    path: String,
    existing: String,
    file: Option<(String, Vec<u8>)>,
    text: String,
}

async fn read_fields(mut multipart: Multipart) -> Result<Fields> {
    let mut fields = Fields::default();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                let file_name = field.file_name().unwrap_or_default().to_string();
                let data = field.bytes().await?.to_vec();
                if !data.is_empty() {
                    fields.file = Some((file_name, data));
                }
            }
            "target" => fields.target = field.text().await?,
            "path" => fields.path = field.text().await?,
            "existing" => fields.existing = field.text().await?,
            "text" => fields.text = field.text().await?,
            _ => {}
        }
    }
    Ok(fields)
}

async fn upload(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, WebError> {
    let fields = read_fields(multipart).await.map_err(WebError)?;
    let Some(target) = Target::parse(&fields.target) else {
        return Ok((StatusCode::BAD_REQUEST, "unknown upload").into_response());
    };
    let Some(path) = inside(&web.daemon.roots, &fields.path) else {
        return Ok(not_found("folder"));
    };
    let existing = Existing::parse(&fields.existing);
    let at = path.clone();
    let result = web
        .with(move |daemon, _| {
            let state = Cache::new(&daemon.state_dir)?;
            let written = match target {
                Target::Artist(kind) => artist_image(&at, kind, fields.file, existing)?,
                Target::Cover => cover(&state, &at, fields.file, existing)?,
                Target::CoverVideo => cover_video(&at, fields.file, existing)?,
                Target::Lyrics => lyrics(&state, &at, fields.file, &fields.text)?,
            };
            record(daemon, Kind::Uploaded, &written);
            Ok(written)
        })
        .await;
    match result {
        Ok(written) => {
            let name = written
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            Ok(back_to(&path, &format!("Uploaded {name}."), &headers))
        }
        // A bad file is the user's to fix: say why, as a 400.
        Err(WebError(err)) => Ok((StatusCode::BAD_REQUEST, format!("{err:#}")).into_response()),
    }
}

/// The image's extension and type, by its first bytes.
fn image_type(data: &[u8]) -> Option<(&'static str, &'static str)> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else {
        None
    }
}

fn image(file: Option<(String, Vec<u8>)>, png_only: bool) -> Result<CachedArt> {
    let (_, data) = file.context("choose a file to upload")?;
    let Some((ext, mime)) = image_type(&data) else {
        bail!("that isn't a JPEG or PNG image");
    };
    if png_only && ext != "png" {
        bail!("logos are PNGs (with a transparent background)");
    }
    let data = crate::lossless::image(&data).unwrap_or(data);
    Ok(CachedArt::uploaded(ext, mime, data))
}

/// An MP4, re-encoded like the downloaded ones when ffmpeg can.
fn video(file: Option<(String, Vec<u8>)>) -> Result<CachedArt> {
    let (_, data) = file.context("choose a file to upload")?;
    if data.get(4..8) != Some(b"ftyp") {
        bail!("that isn't an MP4 video");
    }
    let data = if crate::video::ffmpeg_available() && crate::video::can_shrink() {
        let id = crate::state::digest_bytes(&data);
        let input = std::env::temp_dir().join(format!("gatefold-upload-{id}.mp4"));
        let output = std::env::temp_dir().join(format!("gatefold-upload-{id}-out.mp4"));
        std::fs::write(&input, &data)?;
        let shrunk = crate::video::shrink(input.as_os_str(), None, &output)
            .and_then(|()| std::fs::read(&output).context("reading the re-encoded video"));
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
        // A video ffmpeg can't re-encode is still a video: keep it as is.
        shrunk.unwrap_or(data)
    } else {
        data
    };
    Ok(CachedArt::uploaded("mp4", "video/mp4", data))
}

/// Before an upload replaces `current`: skip its contents when asked.
fn before(current: Option<&Path>, existing: Existing) -> Result<()> {
    if existing == Existing::SkipCurrent
        && let Some(current) = current
    {
        skip::mark(current)?;
    }
    Ok(())
}

/// After: keep the upload when asked (and take back a skip of it).
fn after(written: &Path, existing: Existing) -> Result<()> {
    if existing == Existing::KeepUpload {
        keep::mark_file(written)?;
    } else {
        skip::unmark(written)?;
    }
    Ok(())
}

fn artist_image(
    dir: &Path,
    kind: ArtistImage,
    file: Option<(String, Vec<u8>)>,
    existing: Existing,
) -> Result<PathBuf> {
    if scanner::dir_has_audio(dir) {
        bail!("that's an album folder; artist images go in the artist's folder");
    }
    let art = match kind {
        ArtistImage::Video => video(file)?,
        ArtistImage::Logo => image(file, true)?,
        ArtistImage::Avatar | ArtistImage::Backdrop => image(file, false)?,
    };
    before(kind.existing(dir).as_deref(), existing)?;
    // The current file is archived, dated, so `restore` can bring it back.
    writer::write_artist_image(dir, kind, &art, true, &Revisions::new(), false)?;
    let written = kind.existing(dir).context("the upload wasn't written")?;
    after(&written, existing)?;
    Ok(written)
}

/// `cover.*` in the album folder, and the front cover of every track in it.
fn cover(
    state: &Cache,
    dir: &Path,
    file: Option<(String, Vec<u8>)>,
    existing: Existing,
) -> Result<PathBuf> {
    let art = image(file, false)?;
    let current = writer::existing_cover_sidecar(dir);
    before(current.as_deref(), existing)?;
    // Archive the current `cover.*` (the tracks' embedded covers are backed up
    // as on any replace).
    if let Some(current) = &current {
        let digest = revision::file_digest(current)?;
        Revisions::new().archive(
            &dir.join(writer::ARCHIVE_DIR).join("covers"),
            current,
            &digest,
        )?;
    }
    writer::write_cover_sidecar(dir, &art, true, None, false)?;
    embed_cover_in_tracks(state, dir, &art)?;
    let written = writer::existing_cover_sidecar(dir).context("the upload wasn't written")?;
    after(&written, existing)?;
    Ok(written)
}

fn cover_video(dir: &Path, file: Option<(String, Vec<u8>)>, existing: Existing) -> Result<PathBuf> {
    let art = video(file)?;
    let current = writer::existing_cover_video(dir);
    before(current.as_deref(), existing)?;
    if let Some(current) = &current {
        let digest = revision::file_digest(current)?;
        Revisions::new().archive(
            &dir.join(writer::ARCHIVE_DIR).join("covers"),
            current,
            &digest,
        )?;
    }
    writer::write_cover_video(dir, &art, true, None, false)?;
    let written = writer::existing_cover_video(dir).context("the upload wasn't written")?;
    after(&written, existing)?;
    Ok(written)
}

/// Lyrics for the track at `path`, from a file or pasted text. Synced lyrics
/// become its `.lrc` (the current one archived) and are embedded as plain
/// text; plain lyrics are embedded only.
fn lyrics(
    state: &Cache,
    path: &Path,
    file: Option<(String, Vec<u8>)>,
    text: &str,
) -> Result<PathBuf> {
    if !path.is_file() || !scanner::is_audio(path) {
        bail!("lyrics go with a track");
    }
    let text = match file {
        Some((_, data)) => String::from_utf8(data).context("lyrics must be UTF-8 text")?,
        None => text.to_string(),
    };
    let text = text.replace("\r\n", "\n");
    if text.trim().is_empty() {
        bail!("paste the lyrics or choose a file");
    }
    let dir = path.parent().context("a track without a folder")?;
    let mut written = path.to_path_buf();
    if writer::has_timestamps(&text) {
        let current = scanner::lrc_path_for(path);
        if current.is_file() {
            let digest = revision::file_digest(&current)?;
            Revisions::new().archive(&writer::lyrics_archive_dir(dir), &current, &digest)?;
        }
        let lrc = path.with_extension("lrc");
        std::fs::write(&lrc, text.trim_end().to_string() + "\n")
            .with_context(|| format!("writing {}", lrc.display()))?;
        written = lrc;
    }
    let plain = writer::plain_lyrics(&text);
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = digest_for(&abs);
    let backup = Backup::new(state.backup_root(), &digest, path);
    writer::write_lyrics_tag(path, &plain, true, false, Some(&backup), false)?;
    backup.finish()?;
    Ok(written)
}

/// Embed `art` as the front cover of every track in album folder `dir`
/// (the previous covers backed up, as on any replace).
pub(super) fn embed_cover_in_tracks(state: &Cache, dir: &Path, art: &CachedArt) -> Result<()> {
    for track in crate::album_archive::tracks(dir) {
        let abs = std::fs::canonicalize(&track).unwrap_or_else(|_| track.clone());
        let digest = digest_for(&abs);
        let backup = Backup::new(state.backup_root(), &digest, &track);
        writer::embed_cover(&track, art, true, false, Some(&backup), false)?;
        backup.finish()?;
    }
    Ok(())
}

/// An image file as art to embed.
pub(super) fn art_of(path: &Path) -> Result<CachedArt> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (ext, mime) = image_type(&data).context("not a JPEG or PNG image")?;
    Ok(CachedArt::uploaded(ext, mime, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lyrics_drop_stamps_and_headers() {
        let lrc = "[ar:AFI]\n[00:01.00]Silver and cold\n[00:05.50][01:10.00]Repeat\n\n[00:09.00]";
        assert!(writer::has_timestamps(lrc));
        assert_eq!(writer::plain_lyrics(lrc), "Silver and cold\nRepeat");
        assert!(!writer::has_timestamps("just words"));
    }

    #[test]
    fn uploads_are_checked() {
        assert!(image(Some(("x.gif".into(), b"GIF89a".to_vec())), false).is_err());
        assert!(image(Some(("x.jpg".into(), vec![0xFF, 0xD8, 0xFF, 0xE0])), true).is_err());
        assert!(image(None, false).is_err());
        assert!(video(Some(("x.mp4".into(), b"notavideo".to_vec()))).is_err());
        assert_eq!(Existing::parse("skip_current"), Existing::SkipCurrent);
        assert_eq!(Existing::parse(""), Existing::KeepUpload);
        assert_eq!(
            Target::parse("logo"),
            Some(Target::Artist(ArtistImage::Logo))
        );
        assert_eq!(Target::parse("nope"), None);
    }
}
