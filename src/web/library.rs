//! The library pages: artist folders, each artist's images, archive and
//! history, each album's covers and tracks, and the actions on them (keep,
//! skip, archive, restore). The actions are the same as `gatefold keep`,
//! `skip`, `archive` and `restore`: they write or remove the marker files.

use super::{Web, WebError, data, go_to, render};
use crate::changes::{self, Kind};
use crate::db::ChangeQuery;
use crate::skip;
use crate::writer::{self, ArtistImage};
use crate::{keep, restore, revision::Revisions, scanner};
use askama::Template;
use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/library", get(library_page))
        .route("/artist", get(artist_page))
        .route("/album", get(album_page))
        .route("/file", get(file))
        .route("/actions/file/{action}", post(file_action))
        .route("/actions/folder/{action}", post(folder_action))
        .route("/actions/restore", post(restore_action))
        .route("/actions/restore-album", post(restore_album_action))
        .route("/actions/fix-case", post(fix_case_action))
        .merge(super::upload::routes())
}

/// `path`, canonical, when it's inside one of the library folders.
pub fn inside(roots: &[PathBuf], path: &str) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    roots
        .iter()
        .any(|root| path.starts_with(root))
        .then_some(path)
}

pub(super) fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("no such {what} in the library"),
    )
        .into_response()
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn file_link(path: &Path) -> String {
    format!("/file?path={}", data::encode(&path.display().to_string()))
}

fn size_of(path: &Path) -> String {
    let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes / 1024),
        _ => format!(
            "{}.{} MB",
            bytes / 1_048_576,
            bytes % 1_048_576 * 10 / 1_048_576
        ),
    }
}

// ---- Library -------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct LibraryQuery {
    #[serde(default)]
    q: String,
}

struct ArtistRow {
    name: String,
    link: String,
    thumb: Option<String>,
    albums: usize,
    kept: bool,
    video: bool,
}

#[derive(Template)]
#[template(path = "library.html")]
struct LibraryPage {
    csrf: String,
    query: String,
    artists: Vec<ArtistRow>,
    total: usize,
}

/// The artist folders: the folders directly in each library folder.
fn artist_folders(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| !name_of(p).starts_with('.'))
        .collect();
    dirs.sort_by(|a, b| natord::compare_ignore_case(&name_of(a), &name_of(b)));
    dirs
}

/// The album folders of an artist folder (any depth below it).
fn album_folders(artist: &Path) -> Vec<PathBuf> {
    let mut albums: Vec<PathBuf> = walkdir::WalkDir::new(artist)
        .min_depth(1)
        .max_depth(3)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir() && scanner::dir_has_audio(e.path()))
        .map(walkdir::DirEntry::into_path)
        .collect();
    albums.sort_by(|a, b| natord::compare_ignore_case(&name_of(a), &name_of(b)));
    albums
}

async fn library_page(
    State(web): State<Arc<Web>>,
    Query(params): Query<LibraryQuery>,
) -> Result<Html<String>, WebError> {
    let query = params.q.clone();
    let (artists, total) = web
        .with(move |daemon, _| {
            let all = artist_folders(&daemon.roots);
            let total = all.len();
            let wanted = query.to_lowercase();
            let rows = all
                .into_iter()
                .filter(|dir| wanted.is_empty() || name_of(dir).to_lowercase().contains(&wanted))
                .map(|dir| ArtistRow {
                    name: name_of(&dir),
                    link: data::artist_link(&dir.display().to_string()),
                    thumb: ArtistImage::Avatar.existing(&dir).map(|p| file_link(&p)),
                    albums: std::fs::read_dir(&dir).map_or(0, |entries| {
                        entries
                            .filter_map(Result::ok)
                            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .count()
                    }),
                    kept: dir.join(keep::ALL).is_file(),
                    video: ArtistImage::Video.existing(&dir).is_some(),
                })
                .collect();
            Ok((rows, total))
        })
        .await?;
    render(LibraryPage {
        csrf: web.csrf.clone(),
        query: params.q,
        artists,
        total,
    })
}

// ---- Artist and album pages ---------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct DirQuery {
    dir: String,
    #[serde(default)]
    message: Option<String>,
}

/// One image (or video) gatefold manages, with its state and actions.
#[allow(clippy::struct_excessive_bools)] // independent states, shown as is
struct ImageCard {
    label: &'static str,
    path: String,
    url: Option<String>,
    video: bool,
    size: Option<String>,
    kept: bool,
    skipped: bool,
    /// Archive is for artist files only.
    archivable: bool,
    /// What an upload here replaces (`avatar`, `cover`, …).
    target: &'static str,
}

impl ImageCard {
    /// The file types the upload takes.
    fn accept(&self) -> &'static str {
        match self.target {
            "video" | "cover_video" => "video/mp4",
            "logo" => "image/png",
            _ => "image/jpeg,image/png",
        }
    }
}

impl ImageCard {
    fn of(
        (label, target): (&'static str, &'static str),
        path: Option<PathBuf>,
        archivable: bool,
    ) -> Option<Self> {
        let path = path?;
        let video = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
        Some(Self {
            label,
            url: Some(file_link(&path)),
            video,
            size: Some(size_of(&path)),
            kept: keep::file_is_kept(&path),
            skipped: skip::rejected(&path),
            archivable,
            target,
            path: path.display().to_string(),
        })
    }

    fn missing((label, target): (&'static str, &'static str)) -> Self {
        Self {
            label,
            path: String::new(),
            url: None,
            video: false,
            size: None,
            kept: false,
            skipped: false,
            archivable: false,
            target,
        }
    }
}

struct ArchivedRow {
    label: String,
    path: String,
    url: String,
    video: bool,
    size: String,
    skipped: bool,
}

struct AlbumRow {
    name: String,
    link: String,
    thumb: Option<String>,
    animated: bool,
    tracks: usize,
}

#[derive(Template)]
#[template(path = "artist.html")]
struct ArtistPage {
    csrf: String,
    name: String,
    dir: String,
    shown_dir: String,
    message: Option<String>,
    kept: bool,
    images: Vec<ImageCard>,
    archived: Vec<ArchivedRow>,
    albums: Vec<AlbumRow>,
    history: Vec<data::ChangeGroup>,
    problems: Vec<data::ProblemView>,
}

fn image_cards(dir: &Path) -> Vec<ImageCard> {
    [
        (("Photo", "avatar"), ArtistImage::Avatar),
        (("Backdrop", "backdrop"), ArtistImage::Backdrop),
        (("Motion backdrop", "video"), ArtistImage::Video),
        (("Logo", "logo"), ArtistImage::Logo),
    ]
    .into_iter()
    .map(|(label, kind)| {
        ImageCard::of(label, kind.existing(dir), true).unwrap_or_else(|| ImageCard::missing(label))
    })
    .collect()
}

fn archived_rows(dir: &Path) -> Vec<ArchivedRow> {
    restore::list(dir)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| ArchivedRow {
            label: name_of(&entry.path),
            url: file_link(&entry.path),
            video: entry.kind == ArtistImage::Video,
            size: size_of(&entry.path),
            skipped: skip::rejects(
                &entry.kind.target_dir(dir),
                &std::fs::read(&entry.path).unwrap_or_default(),
            ),
            path: entry.path.display().to_string(),
        })
        .collect()
}

fn album_rows(artist: &Path) -> Vec<AlbumRow> {
    album_folders(artist)
        .into_iter()
        .map(|dir| AlbumRow {
            name: dir
                .strip_prefix(artist)
                .map_or_else(|_| name_of(&dir), |p| p.display().to_string()),
            link: data::album_link(&dir),
            thumb: writer::existing_cover_sidecar(&dir).map(|p| file_link(&p)),
            animated: writer::existing_cover_video(&dir).is_some(),
            tracks: std::fs::read_dir(&dir).map_or(0, |entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| scanner::is_audio(&e.path()))
                    .count()
            }),
        })
        .collect()
}

async fn artist_page(
    State(web): State<Arc<Web>>,
    Query(params): Query<DirQuery>,
) -> Result<Response, WebError> {
    let Some(dir) = inside(&web.daemon.roots, &params.dir) else {
        return Ok(not_found("artist folder"));
    };
    let csrf = web.csrf.clone();
    let page = web
        .with(move |daemon, db| {
            let history = db.changes(&ChangeQuery {
                artist_dir: Some(&dir),
                limit: 200,
                ..ChangeQuery::default()
            })?;
            Ok(ArtistPage {
                csrf,
                name: name_of(&dir),
                shown_dir: super::display_path(&daemon.roots, &dir),
                message: params.message,
                kept: dir.join(keep::ALL).is_file(),
                images: image_cards(&dir),
                archived: archived_rows(&dir),
                albums: album_rows(&dir),
                history: data::group(&history),
                problems: data::problem_views(&daemon.roots, &crate::casefix::scan(&dir)),
                dir: dir.display().to_string(),
            })
        })
        .await?;
    Ok(render(page)?.into_response())
}

/// The album page's cards: label and upload target.
const COVER: (&str, &str) = ("Cover", "cover");
const ANIMATED: (&str, &str) = ("Animated cover", "cover_video");

struct TrackRow {
    path: String,
    number: String,
    title: String,
    duration: String,
    /// `synced`, `plain` or `none`: the `.lrc` next to the track.
    lyrics: &'static str,
}

#[derive(Template)]
#[template(path = "album.html")]
struct AlbumPage {
    csrf: String,
    name: String,
    dir: String,
    shown_dir: String,
    artist_link: String,
    artist_name: String,
    message: Option<String>,
    kept: bool,
    covers: Vec<ImageCard>,
    tracks: Vec<TrackRow>,
    history: Vec<data::ChangeGroup>,
    archived: Vec<AlbumArchivedRow>,
}

struct AlbumArchivedRow {
    /// What it's a version of: "Cover", "Animated cover", "Lyrics: 01 - Song".
    label: String,
    name: String,
    path: String,
    url: String,
    /// `image`, `video` or `lyrics`, for the thumbnail.
    shown_as: &'static str,
    size: String,
    /// Covers can be kept once restored; lyrics have no keep marker.
    keepable: bool,
}

fn album_archived_rows(dir: &Path) -> Vec<AlbumArchivedRow> {
    crate::album_archive::list(dir)
        .into_iter()
        .map(|entry| {
            let (label, shown_as, keepable) = match &entry.kind {
                crate::album_archive::Kind::Cover => ("Cover".to_string(), "image", true),
                crate::album_archive::Kind::CoverVideo => {
                    ("Animated cover".to_string(), "video", true)
                }
                crate::album_archive::Kind::Lyrics(stem) => {
                    (format!("Lyrics: {stem}"), "lyrics", false)
                }
            };
            AlbumArchivedRow {
                label,
                name: name_of(&entry.path),
                url: file_link(&entry.path),
                shown_as,
                size: size_of(&entry.path),
                keepable,
                path: entry.path.display().to_string(),
            }
        })
        .collect()
}

fn track_rows(dir: &Path) -> Vec<TrackRow> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file() && scanner::is_audio(p))
                .collect()
        },
    );
    files.sort_by(|a, b| natord::compare(&a.to_string_lossy(), &b.to_string_lossy()));
    let mut rows: Vec<(Option<(u32, u32)>, TrackRow)> = files
        .iter()
        .map(|path| {
            let lrc = scanner::lrc_path_for(path);
            let lyrics = if !lrc.is_file() {
                "none"
            } else if writer::lrc_is_unsynced(&lrc) {
                "plain"
            } else {
                "synced"
            };
            match crate::tracklist::read(path) {
                Ok(entry) => (
                    entry.track.map(|t| (entry.disc.unwrap_or(1), t)),
                    TrackRow {
                        path: path.display().to_string(),
                        number: entry.track.map_or_else(String::new, |t| t.to_string()),
                        title: entry.title,
                        duration: crate::tracklist::format_duration(entry.duration),
                        lyrics,
                    },
                ),
                Err(_) => (
                    None,
                    TrackRow {
                        path: path.display().to_string(),
                        number: String::new(),
                        title: name_of(path),
                        duration: String::new(),
                        lyrics,
                    },
                ),
            }
        })
        .collect();
    rows.sort_by_key(|(order, _)| order.unwrap_or((u32::MAX, u32::MAX)));
    rows.into_iter().map(|(_, row)| row).collect()
}

async fn album_page(
    State(web): State<Arc<Web>>,
    Query(params): Query<DirQuery>,
) -> Result<Response, WebError> {
    let Some(dir) = inside(&web.daemon.roots, &params.dir) else {
        return Ok(not_found("album folder"));
    };
    let csrf = web.csrf.clone();
    let page = web
        .with(move |daemon, db| {
            let history = db.changes(&ChangeQuery {
                artist_dir: Some(&dir),
                limit: 200,
                ..ChangeQuery::default()
            })?;
            let artist = dir.parent().unwrap_or(&dir).to_path_buf();
            Ok(AlbumPage {
                csrf,
                name: name_of(&dir),
                shown_dir: super::display_path(&daemon.roots, &dir),
                artist_link: data::artist_link(&artist.display().to_string()),
                artist_name: name_of(&artist),
                message: params.message,
                kept: dir.join(keep::ALL).is_file() || dir.join(keep::COVER).is_file(),
                covers: vec![
                    ImageCard::of(COVER, writer::existing_cover_sidecar(&dir), false)
                        .unwrap_or_else(|| ImageCard::missing(COVER)),
                    ImageCard::of(ANIMATED, writer::existing_cover_video(&dir), false)
                        .unwrap_or_else(|| ImageCard::missing(ANIMATED)),
                ],
                tracks: track_rows(&dir),
                history: data::group(&history),
                archived: album_archived_rows(&dir),
                dir: dir.display().to_string(),
            })
        })
        .await?;
    Ok(render(page)?.into_response())
}

// ---- Actions -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PathForm {
    path: String,
    #[serde(default)]
    keep: Option<String>,
}

/// Back to the page the file belongs on, saying `message`.
pub(super) fn back_to(path: &Path, message: &str, headers: &HeaderMap) -> Response {
    let page = match skip::classify(path) {
        Some((skip::Managed::Cover, album)) => data::album_link(&album),
        Some((skip::Managed::Artist(_), artist)) => {
            data::artist_link(&artist.display().to_string())
        }
        None if scanner::dir_has_audio(path) => data::album_link(path),
        None => data::artist_link(&path.display().to_string()),
    };
    go_to(
        &format!("{page}&message={}", data::encode(message)),
        headers,
    )
}

/// Record a change made from the UI, and tell the live page.
pub(super) fn record(daemon: &crate::daemon::Daemon, kind: Kind, path: &Path) {
    let change = changes::for_file(kind, changes::Action::Created, path);
    if let Err(err) = daemon
        .db()
        .and_then(|db| db.insert_change(None, crate::db::now(), &change))
    {
        eprintln!("warning: couldn't record the change in gatefold.db: {err:#}");
    }
    crate::events::emit(crate::events::Event::Changed);
}

async fn file_action(
    State(web): State<Arc<Web>>,
    axum::extract::Path(action): axum::extract::Path<String>,
    headers: HeaderMap,
    Form(form): Form<PathForm>,
) -> Result<Response, WebError> {
    let Some(path) = inside(&web.daemon.roots, &form.path) else {
        return Ok(not_found("file"));
    };
    if skip::classify(&path).is_none() {
        return Ok((StatusCode::BAD_REQUEST, "not a file gatefold manages").into_response());
    }
    let target = path.clone();
    let message = web
        .with(move |daemon, _| {
            let path = &target;
            let name = name_of(path);
            let message = match action.as_str() {
                "keep" => {
                    keep::mark_file(path)?;
                    record(daemon, Kind::Kept, path);
                    format!("Keeping {name}: replace runs leave it alone.")
                }
                "unkeep" => {
                    if keep::unmark_file(path)?.is_some() {
                        record(daemon, Kind::Unkept, path);
                    }
                    if keep::file_is_kept(path) {
                        format!("{name} is still kept by its folder's {}.", keep::ALL)
                    } else {
                        format!("No longer keeping {name}.")
                    }
                }
                "skip" => {
                    skip::mark(path)?;
                    record(daemon, Kind::Skipped, path);
                    format!(
                        "Skipped {name}: it's replaced as soon as a different one is available."
                    )
                }
                "unskip" => {
                    if skip::unmark(path)?.is_some() {
                        record(daemon, Kind::Unskipped, path);
                    }
                    format!("{name} is welcome again.")
                }
                "archive" => {
                    let moved = crate::archive::file(path, &Revisions::new())?;
                    record(daemon, Kind::Archived, path);
                    format!("Archived {name} as {}.", moved.archived)
                }
                _ => anyhow::bail!("unknown action {action}"),
            };
            Ok(message)
        })
        .await?;
    Ok(back_to(&path, &message, &headers))
}

#[derive(Debug, Deserialize)]
struct DirForm {
    dir: String,
}

async fn folder_action(
    State(web): State<Arc<Web>>,
    axum::extract::Path(action): axum::extract::Path<String>,
    headers: HeaderMap,
    Form(form): Form<DirForm>,
) -> Result<Response, WebError> {
    let Some(dir) = inside(&web.daemon.roots, &form.dir).filter(|d| d.is_dir()) else {
        return Ok(not_found("folder"));
    };
    let target = dir.clone();
    let message = web
        .with(move |daemon, _| {
            let dir = &target;
            Ok(match action.as_str() {
                "keep" => {
                    keep::mark_dir(dir)?;
                    record(daemon, Kind::Kept, dir);
                    "Keeping everything here: replace runs leave it alone.".to_string()
                }
                "unkeep" => {
                    if keep::unmark_dir(dir)?.is_some() {
                        record(daemon, Kind::Unkept, dir);
                    }
                    "No longer keeping everything here.".to_string()
                }
                _ => anyhow::bail!("unknown action {action}"),
            })
        })
        .await?;
    Ok(back_to(&dir, &message, &headers))
}

async fn restore_action(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<PathForm>,
) -> Result<Response, WebError> {
    let Some(path) = inside(&web.daemon.roots, &form.path) else {
        return Ok(not_found("archived file"));
    };
    let (Some(kind), Some(dir)) = (
        restore::kind_of(&path),
        restore::artist_dir_of(&path).map(Path::to_path_buf),
    ) else {
        return Ok((StatusCode::BAD_REQUEST, "not an archived artist file").into_response());
    };
    let keep_it = form.keep.as_deref().is_some_and(|v| !v.is_empty());
    let artist = dir.clone();
    let message = web
        .with(move |daemon, _| {
            let entry = restore::Entry { kind, path };
            let restored = restore::restore(&artist, &entry, &Revisions::new())?;
            record(daemon, Kind::Restored, &restored.current);
            if keep_it {
                keep::mark_artist_image(&artist, kind)?;
                record(daemon, Kind::Kept, &restored.current);
            }
            Ok(format!(
                "Restored {}{}.",
                name_of(&restored.current),
                if keep_it { " and kept it" } else { "" }
            ))
        })
        .await?;
    let page = data::artist_link(&dir.display().to_string());
    Ok(go_to(
        &format!("{page}&message={}", data::encode(&message)),
        &headers,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct MaybeDirForm {
    #[serde(default)]
    dir: Option<String>,
}

/// Fix every file name in the wrong case under a folder (or the library).
async fn fix_case_action(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<MaybeDirForm>,
) -> Result<Response, WebError> {
    let dir = match form.dir.as_deref().filter(|d| !d.is_empty()) {
        Some(dir) => match inside(&web.daemon.roots, dir) {
            Some(dir) => Some(dir),
            None => return Ok(not_found("folder")),
        },
        None => None,
    };
    let scope = dir.clone();
    let message = web
        .with(move |daemon, _| {
            let roots = scope
                .clone()
                .map_or_else(|| daemon.roots.clone(), |dir| vec![dir]);
            let revisions = Revisions::new();
            let (mut fixed, mut failed) = (0, 0);
            for problem in roots.iter().flat_map(|root| crate::casefix::scan(root)) {
                match crate::casefix::fix(&problem, &revisions) {
                    Ok(done) => {
                        fixed += 1;
                        record(daemon, Kind::FixedCase, &done.kept);
                    }
                    Err(err) => {
                        failed += 1;
                        eprintln!("✗ {}: {err:#}", problem.expected.display());
                    }
                }
            }
            daemon.check_case();
            Ok(match failed {
                0 => format!("Fixed {fixed} file name(s)."),
                _ => {
                    format!("Fixed {fixed} file name(s); {failed} couldn't be fixed (see the log).")
                }
            })
        })
        .await?;
    match dir {
        Some(dir) => Ok(back_to(&dir, &message, &headers)),
        None => Ok(super::status_with(&web, Some(message))
            .await?
            .into_response()),
    }
}

/// Bring back a cover, animated cover or `.lrc` from an album folder's
/// archive. A restored cover is embedded in every track again.
async fn restore_album_action(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<PathForm>,
) -> Result<Response, WebError> {
    let Some(path) = inside(&web.daemon.roots, &form.path) else {
        return Ok(not_found("archived file"));
    };
    // `<album>/.gatefold-archive/<section>/<file>`
    let Some(dir) = path
        .parent()
        .and_then(Path::parent)
        .filter(|archive| {
            archive
                .file_name()
                .is_some_and(|n| n == writer::ARCHIVE_DIR)
        })
        .and_then(Path::parent)
        .map(Path::to_path_buf)
    else {
        return Ok((StatusCode::BAD_REQUEST, "not an archived album file").into_response());
    };
    let Some(entry) = crate::album_archive::entry(&dir, &path) else {
        return Ok((StatusCode::BAD_REQUEST, "not an archived album file").into_response());
    };
    let keep_it = form.keep.as_deref().is_some_and(|v| !v.is_empty())
        && !matches!(entry.kind, crate::album_archive::Kind::Lyrics(_));
    let album = dir.clone();
    let message = web
        .with(move |daemon, _| {
            let restored = crate::album_archive::restore(&album, &entry, &Revisions::new())?;
            if entry.kind == crate::album_archive::Kind::Cover {
                let state = crate::state::State::new(&daemon.state_dir)?;
                let art = super::upload::art_of(&restored)?;
                super::upload::embed_cover_in_tracks(&state, &album, &art)?;
            }
            record(daemon, Kind::Restored, &restored);
            if keep_it {
                keep::mark_file(&restored)?;
                record(daemon, Kind::Kept, &restored);
            }
            Ok(format!(
                "Restored {}{}.",
                name_of(&restored),
                if keep_it { " and kept it" } else { "" }
            ))
        })
        .await?;
    Ok(go_to(
        &format!(
            "{}&message={}",
            data::album_link(&dir),
            data::encode(&message)
        ),
        &headers,
    ))
}

// ---- Files ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
}

/// An image or video from the library (thumbnails, previews). Byte ranges are
/// honoured, so videos play and seek.
async fn file(
    State(web): State<Arc<Web>>,
    Query(params): Query<FileQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(path) = inside(&web.daemon.roots, &params.path).filter(|p| p.is_file()) else {
        return not_found("file");
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(String::new, str::to_ascii_lowercase);
    let kind = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "mp4" if !scanner::is_audio(&path) => "video/mp4",
        "lrc" => "text/plain; charset=utf-8",
        _ => return not_found("image"),
    };
    let Ok(data) = tokio::fs::read(&path).await else {
        return not_found("file");
    };
    let total = data.len();
    let cache = (header::CACHE_CONTROL, "private, max-age=60");
    if let Some((start, end)) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total))
    {
        return (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, kind.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                ),
                (cache.0, cache.1.to_string()),
            ],
            data[start..=end].to_vec(),
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::ACCEPT_RANGES, "bytes"),
            cache,
        ],
        data,
    )
        .into_response()
}

/// `bytes=START-END` (or `START-`, or `-SUFFIX`), within `total`.
fn parse_range(value: &str, total: usize) -> Option<(usize, usize)> {
    let spec = value.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (start, end) = spec.split_once('-')?;
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let (start, end) = if start.is_empty() {
        let suffix: usize = end.parse().ok()?;
        (total.saturating_sub(suffix), last)
    } else {
        let start: usize = start.parse().ok()?;
        let end = if end.is_empty() {
            last
        } else {
            end.parse::<usize>().ok()?.min(last)
        };
        (start, end)
    };
    (start <= end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=500-100", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }
}
