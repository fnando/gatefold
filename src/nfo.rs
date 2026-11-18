//! `.nfo` files Jellyfin (and Kodi) read, from what the tags say:
//!
//! - `artist.nfo` in an artist folder: the artist's name and `MusicBrainz` id, so
//!   Jellyfin matches the folder to the right artist;
//! - `album.nfo` in an album folder: everything the tags know about the
//!   release, and every track.
//!
//! An existing file is only ever completed: missing elements are added right
//! after its root element, and everything else is left byte for byte.

use crate::artist::normalize_name;
use crate::metadata::TrackMetadata;
use crate::paths::write_atomic;
use crate::sample::most_common;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const ARTIST_FILE: &str = "artist.nfo";
pub const ALBUM_FILE: &str = "album.nfo";

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>";
const ARTIST_ID: &str = "musicbrainzartistid";
const ALBUM_ID: &str = "musicbrainzalbumid";

/// Folder names (after [`normalize_name`]) that hold compilations, not an artist.
const COMPILATION_NAMES: &[&str] = &[
    "variousartists",
    "va",
    "compilation",
    "compilations",
    "soundtrack",
    "soundtracks",
    "ost",
];

/// What writing an nfo file did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Wrote,
    /// An existing file got the elements it was missing.
    Completed,
    Already,
    /// The existing file names this other `MusicBrainz` id; left as is.
    Differs(String),
    /// The existing file has no `<artist>` / `<album>` element; left as is.
    WrongRoot,
}

/// Why an artist folder gets no `artist.nfo`.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    /// Albums of several album artists live in it.
    Shared,
    /// Named like a compilations folder.
    Compilation,
    /// The artist's name (from `MusicBrainz`, else the tag) differs from the
    /// folder's (a joint album artist tagged with one artist's id, say).
    OtherName(String),
}

/// The name to write in artist folder `folder_name`'s `artist.nfo`, or why it
/// gets none: `claims` is how many album artists found it their folder,
/// `mb_name` the `MusicBrainz` artist's name when known, `tagged` the album
/// artist as tagged. The tag can be a joint credit ("A & B") tagged with one
/// artist's id, so the `MusicBrainz` name wins, and either must match the folder.
pub fn artist_title<'a>(
    folder_name: &str,
    mb_name: Option<&'a str>,
    tagged: &'a str,
    claims: usize,
) -> Result<&'a str, Skip> {
    let folder = normalize_name(folder_name);
    if claims > 1 {
        return Err(Skip::Shared);
    }
    if COMPILATION_NAMES.contains(&folder.as_str()) {
        return Err(Skip::Compilation);
    }
    let name = mb_name.unwrap_or(tagged);
    if normalize_name(name) == folder {
        Ok(name)
    } else {
        Err(Skip::OtherName(name.to_string()))
    }
}

/// One top-level element to write: its tag (to tell whether a file already has
/// one) and its markup, possibly several lines (repeated or nested elements).
struct Element {
    tag: &'static str,
    xml: String,
}

impl Element {
    fn text(tag: &'static str, value: &str) -> Self {
        Self {
            tag,
            xml: element(tag, value),
        }
    }
}

/// Write `artist.nfo` in `folder` naming the artist and its `MusicBrainz` id,
/// or complete an existing one with whichever of the two it lacks. On a
/// `dry_run`, says what it would do and writes nothing.
pub fn write_artist(folder: &Path, name: &str, mbid: &str, dry_run: bool) -> Result<Outcome> {
    let elements = [Element::text("title", name), Element::text(ARTIST_ID, mbid)];
    write(
        &folder.join(ARTIST_FILE),
        "artist",
        (ARTIST_ID, mbid),
        &elements,
        dry_run,
    )
}

/// Write `path` with `root` holding `elements`, or complete an existing file
/// with the elements it has none of. `id` is the element that names the
/// subject: an existing file naming another one is left alone.
fn write(
    path: &Path,
    root: &str,
    id: (&str, &str),
    elements: &[Element],
    dry_run: bool,
) -> Result<Outcome> {
    let existing = match fs::read(path) {
        Ok(bytes) => Some(
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?,
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let Some(existing) = existing else {
        if !dry_run {
            let lines: Vec<String> = elements.iter().map(|e| indent(&e.xml, "\n")).collect();
            let body = lines.join("\n");
            let xml = format!("{HEADER}\n<{root}>\n{body}\n</{root}>\n");
            write_atomic(path, xml.as_bytes())?;
        }
        return Ok(Outcome::Wrote);
    };
    Ok(match complete(&existing, root, id, elements) {
        Completion::Changed(xml) => {
            if !dry_run {
                write_atomic(path, xml.as_bytes())?;
            }
            Outcome::Completed
        }
        Completion::Already => Outcome::Already,
        Completion::Differs(id) => Outcome::Differs(id),
        Completion::WrongRoot => Outcome::WrongRoot,
    })
}

enum Completion {
    Changed(String),
    Already,
    Differs(String),
    WrongRoot,
}

/// `xml` with the elements it has none of inserted right after `<root>`.
/// Only top-level elements count: a `<title>` inside a `<track>` isn't the
/// album's.
fn complete(xml: &str, root: &str, (id_tag, id): (&str, &str), elements: &[Element]) -> Completion {
    let Some(open_end) = open_tag_end(xml, root) else {
        return Completion::WrongRoot;
    };
    let top = without_tracks(xml);
    if let Some(found) = value(&top, id_tag)
        && !found.eq_ignore_ascii_case(id)
    {
        return Completion::Differs(found);
    }
    let newline = if xml.contains("\r\n") { "\r\n" } else { "\n" };
    let missing: Vec<String> = elements
        .iter()
        .filter(|e| {
            // `<track>` itself is what was blanked out of `top`.
            let within = if e.tag == "track" { xml } else { &top };
            open_tag_end(within, e.tag).is_none()
        })
        .map(|e| indent(&e.xml, newline))
        .collect();
    let insert = if missing.is_empty() {
        String::new()
    } else {
        format!("{newline}{}", missing.join(newline))
    };
    if insert.is_empty() {
        return Completion::Already;
    }
    Completion::Changed(format!("{}{insert}{}", &xml[..open_end], &xml[open_end..]))
}

/// `xml` (one or more lines) indented two spaces, with `newline` between lines.
fn indent(xml: &str, newline: &str) -> String {
    xml.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join(newline)
}

/// `xml` with every `<track>…</track>` blanked out (same length, so offsets
/// still line up), for looking at top-level elements only.
fn without_tracks(xml: &str) -> String {
    let mut out = xml.to_string();
    let lower = xml.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = open_tag_end(&lower[from..], "track").map(|end| from + end) {
        let open = lower[..start].rfind("<track").unwrap_or(start);
        let Some(close) = lower[start..].find("</track>").map(|at| start + at + 8) else {
            break;
        };
        out.replace_range(open..close, &" ".repeat(close - open));
        from = close;
    }
    out
}

fn element(tag: &str, text: &str) -> String {
    format!("<{tag}>{}</{tag}>", escape(text))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Byte offset just past the first `<tag>` (or `<tag …>`) opening tag, ignoring
/// ASCII case.
fn open_tag_end(xml: &str, tag: &str) -> Option<usize> {
    let lower = xml.to_ascii_lowercase();
    let needle = format!("<{tag}");
    let mut from = 0;
    while let Some(at) = lower[from..].find(&needle) {
        let after = from + at + needle.len();
        match lower.as_bytes().get(after) {
            Some(b'>') => return Some(after + 1),
            Some(c) if c.is_ascii_whitespace() => {
                return lower[after..].find('>').map(|end| after + end + 1);
            }
            _ => from = after,
        }
    }
    None
}

/// The trimmed text of the first `<tag>` element, if it has any.
fn value(xml: &str, tag: &str) -> Option<String> {
    let start = open_tag_end(xml, tag)?;
    let close = format!("</{tag}>");
    let end = xml.to_ascii_lowercase()[start..].find(&close)?;
    let text = xml[start..start + end].trim();
    (!text.is_empty()).then(|| text.to_string())
}

// ---- album.nfo ----------------------------------------------------------------

/// An album as its tracks' tags describe it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Album {
    pub title: Option<String>,
    /// The album artist credit, as tagged (`ALBUMARTIST`): only to tell which
    /// tracks are credited the same, never written itself.
    pub credit: Option<String>,
    /// Each album artist (`ALBUMARTISTS`), else the credit.
    pub artists: Vec<String>,
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    /// Each album artist's `MusicBrainz` id, in the order of [`Self::artists`];
    /// empty unless there's exactly one per artist (see [`credited`]).
    pub artist_ids: Vec<String>,
    pub year: Option<String>,
    pub date: Option<String>,
    pub original_date: Option<String>,
    pub genres: Vec<String>,
    pub labels: Vec<String>,
    pub catalog_number: Option<String>,
    pub barcode: Option<String>,
    pub release_type: Option<String>,
    pub release_country: Option<String>,
    pub media: Option<String>,
    pub compilation: Option<bool>,
    pub total_discs: Option<u32>,
    /// Whole minutes, summed over the tracks.
    pub runtime: Option<u64>,
    pub tracks: Vec<AlbumTrack>,
}

/// One track of an [`Album`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AlbumTrack {
    pub disc: Option<u32>,
    pub position: Option<u32>,
    pub title: Option<String>,
    /// Only when it differs from the album's: the credit as tagged (`ARTIST`),
    /// or each artist (`ARTISTS`) on an album credited to several.
    pub artists: Vec<String>,
    /// Seconds.
    pub duration: Option<u64>,
    pub recording_id: Option<String>,
    pub release_track_id: Option<String>,
    pub isrc: Option<String>,
}

/// The album `tracks` (all of one release) describe. Album-wide values are the
/// most common one among the tracks that have it, so one oddly tagged track
/// doesn't win. Tracks are in disc and track order, unnumbered ones last (by
/// file name).
pub fn album_from(tracks: &[&TrackMetadata]) -> Album {
    let text = |get: fn(&TrackMetadata) -> Option<&str>| {
        most_common(
            tracks
                .iter()
                .filter_map(|t| get(t))
                .filter(|v| !v.trim().is_empty()),
        )
        .map(str::to_string)
    };
    let list = |get: fn(&TrackMetadata) -> &[String]| {
        most_common(tracks.iter().map(|t| get(t)).filter(|v| !v.is_empty()))
            .map_or_default(<[String]>::to_vec)
    };
    let credit = text(|t| t.album_artist_name.as_deref());
    let (artists, artist_ids) = credited(
        credit.as_deref(),
        list(|t| &t.release.album_artists),
        list(|t| &t.release.album_artist_ids),
    );
    let joint = artists.len() > 1;
    let date = text(|t| t.release.date.as_deref());
    let original_date = text(|t| t.release.original_date.as_deref());
    let year = original_date
        .as_deref()
        .or(date.as_deref())
        .and_then(|d| d.get(..4))
        .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string);
    let total_discs = most_common(tracks.iter().filter_map(|t| t.release.disc_total))
        .or_else(|| tracks.iter().filter_map(|t| t.release.disc_number).max());
    let seconds: u64 = tracks.iter().filter_map(|t| t.duration).sum();

    let mut ordered: Vec<&&TrackMetadata> = tracks.iter().collect();
    ordered.sort_by(|a, b| {
        let key = |t: &TrackMetadata| {
            (
                t.release.track_number.is_none(),
                t.release.disc_number.unwrap_or(1),
                t.release.track_number,
                file_name(&t.path),
            )
        };
        key(a).cmp(&key(b))
    });
    let album_tracks = ordered
        .into_iter()
        .map(|t| AlbumTrack {
            disc: t.release.disc_number,
            position: t.release.track_number,
            title: t.track_name.clone().filter(|v| !v.trim().is_empty()),
            artists: track_artists(t, credit.as_deref(), &artists, joint),
            duration: t.duration,
            recording_id: t.release.recording_id.clone(),
            release_track_id: t.release.release_track_id.clone(),
            isrc: t.release.isrc.clone(),
        })
        .collect();

    Album {
        title: text(|t| t.album_name.as_deref()),
        credit,
        artists,
        release_id: text(|t| t.musicbrainz_release_id.as_deref()),
        release_group_id: text(|t| t.musicbrainz_release_group_id.as_deref()),
        artist_ids,
        year,
        date,
        original_date,
        genres: every(tracks.iter().map(|t| t.release.genres.as_slice())),
        labels: list(|t| &t.release.labels),
        catalog_number: text(|t| t.release.catalog_number.as_deref()),
        barcode: text(|t| t.barcode.as_deref()),
        release_type: text(|t| t.release.release_type.as_deref()),
        release_country: text(|t| t.release.release_country.as_deref()),
        media: text(|t| t.release.media.as_deref()),
        compilation: most_common(tracks.iter().filter_map(|t| t.release.compilation)),
        total_discs,
        runtime: (seconds > 0).then(|| (seconds + 30) / 60),
        tracks: album_tracks,
    }
}

/// The album artist tag Jellyfin misreads, if the album's tracks carry one: a
/// single `ALBUMARTIST` value ("A + B") next to several album artist ids, which
/// Jellyfin reads as the first artist's name, renaming that artist. Returns the
/// value and the id count.
pub fn joint_album_artist_tag(tracks: &[&TrackMetadata]) -> Option<(String, usize)> {
    tracks.iter().find_map(|t| {
        let ids = t.release.album_artist_ids.len();
        match t.release.album_artist_values.as_slice() {
            [one] if ids > 1 => Some((one.clone(), ids)),
            _ => None,
        }
    })
}

/// The album artists to write and their `MusicBrainz` ids, from the credit as
/// tagged, each album artist (`names`) and the ids (`ids`, in credit order). A
/// name and an id are only ever written together when they're the same
/// artist's: one id per name, or a lone credit with a lone id. A credit naming
/// several artists ("A + B") with several ids keeps its name but gets no id,
/// rather than the first artist's.
fn credited(
    credit: Option<&str>,
    names: Vec<String>,
    ids: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let names = if names.is_empty() {
        credit.map(str::to_string).into_iter().collect()
    } else {
        names
    };
    if names.is_empty() || names.len() == ids.len() {
        (names, ids)
    } else {
        (names, Vec::new())
    }
}

/// A track's artists, when they differ from the album's: on an album credited
/// to several artists (`joint`), each of the track's (`ARTISTS`) when tagged;
/// otherwise the credit as tagged ("A feat. B" stays as is).
fn track_artists(
    track: &TrackMetadata,
    credit: Option<&str>,
    album: &[String],
    joint: bool,
) -> Vec<String> {
    let each = &track.release.artists;
    if joint && each.len() > 1 {
        return if each.as_slice() == album {
            Vec::new()
        } else {
            each.clone()
        };
    }
    let Some(name) = track.artist_name.as_deref().map(str::trim) else {
        return Vec::new();
    };
    let same = Some(name) == credit.map(str::trim) || [name] == album;
    if name.is_empty() || same {
        Vec::new()
    } else {
        vec![name.to_string()]
    }
}

/// Every distinct value across `lists`, most frequent first (then first seen).
fn every<'a>(lists: impl Iterator<Item = &'a [String]>) -> Vec<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for value in lists.flatten() {
        match counts.iter_mut().find(|(seen, _)| *seen == value.as_str()) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    // A stable sort keeps first-seen order among equals.
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts
        .into_iter()
        .map(|(value, _)| value.to_string())
        .collect()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned())
}

/// Write `album.nfo` in `folder` for `album`, or complete an existing one
/// (its `<track>` list only when it has none). On a `dry_run`, says what it
/// would do and writes nothing.
pub fn write_album(folder: &Path, album: &Album, dry_run: bool) -> Result<Outcome> {
    let id = album.release_id.as_deref().unwrap_or_default();
    write(
        &folder.join(ALBUM_FILE),
        "album",
        (ALBUM_ID, id),
        &album_elements(album),
        dry_run,
    )
}

fn album_elements(album: &Album) -> Vec<Element> {
    let mut out = Vec::new();
    let a = album;
    text(&mut out, "title", a.title.as_deref());
    repeated(&mut out, "artist", &a.artists);
    repeated(&mut out, "albumartist", &a.artists);
    text(&mut out, ALBUM_ID, a.release_id.as_deref());
    text(
        &mut out,
        "musicbrainzreleasegroupid",
        a.release_group_id.as_deref(),
    );
    repeated(&mut out, "musicbrainzalbumartistid", &a.artist_ids);
    text(&mut out, "year", a.year.as_deref());
    text(&mut out, "premiered", a.date.as_deref());
    text(&mut out, "releasedate", a.date.as_deref());
    text(&mut out, "originalreleasedate", a.original_date.as_deref());
    repeated(&mut out, "genre", &a.genres);
    repeated(&mut out, "label", &a.labels);
    // Jellyfin reads labels as studios.
    repeated(&mut out, "studio", &a.labels);
    text(&mut out, "catalognumber", a.catalog_number.as_deref());
    text(&mut out, "barcode", a.barcode.as_deref());
    text(&mut out, "releasetype", a.release_type.as_deref());
    text(&mut out, "releasecountry", a.release_country.as_deref());
    text(&mut out, "media", a.media.as_deref());
    text(
        &mut out,
        "compilation",
        a.compilation.map(|c| c.to_string()).as_deref(),
    );
    text(
        &mut out,
        "totaldiscs",
        a.total_discs.map(|n| n.to_string()).as_deref(),
    );
    text(
        &mut out,
        "runtime",
        a.runtime.map(|n| n.to_string()).as_deref(),
    );
    if !a.tracks.is_empty() {
        let tracks: Vec<String> = a.tracks.iter().map(track_xml).collect();
        out.push(Element {
            tag: "track",
            xml: tracks.join("\n"),
        });
    }
    out
}

/// Add `<tag>value</tag>` when there's a value.
fn text(out: &mut Vec<Element>, tag: &'static str, value: Option<&str>) {
    if let Some(value) = value {
        out.push(Element::text(tag, value));
    }
}

/// Add one `<tag>` per value, as a single element (all or nothing).
fn repeated(out: &mut Vec<Element>, tag: &'static str, values: &[String]) {
    if !values.is_empty() {
        let lines: Vec<String> = values.iter().map(|v| element(tag, v)).collect();
        out.push(Element {
            tag,
            xml: lines.join("\n"),
        });
    }
}

fn track_xml(track: &AlbumTrack) -> String {
    let mut lines = vec!["<track>".to_string()];
    let mut push = |tag: &str, value: Option<String>| {
        if let Some(value) = value {
            lines.push(format!("  {}", element(tag, &value)));
        }
    };
    push("disc", track.disc.map(|n| n.to_string()));
    push("position", track.position.map(|n| n.to_string()));
    push("title", track.title.clone());
    for artist in &track.artists {
        push("artist", Some(artist.clone()));
    }
    push(
        "duration",
        track.duration.map(|s| format!("{}:{:02}", s / 60, s % 60)),
    );
    push("musicbrainztrackid", track.recording_id.clone());
    push("musicbrainzreleasetrackid", track.release_track_id.clone());
    push("isrc", track.isrc.clone());
    lines.push("</track>".to_string());
    lines.join("\n")
}

/// Album folders with their tracks, and the folders skipped.
pub type AlbumFolders<'a> = (
    Vec<(PathBuf, Vec<&'a TrackMetadata>)>,
    Vec<(PathBuf, AlbumSkip)>,
);

/// Why a folder gets no `album.nfo`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlbumSkip {
    /// Tracks of more than one album live in it.
    Mixed,
    /// It's the scan root or an artist folder (loose tracks).
    NotAnAlbumFolder,
}

/// The album folders among `tracks` (path, tags), each with its tracks, and
/// the folders skipped. A release (`MusicBrainz` release id) lives in the
/// deepest folder holding all its tracks, so disc subfolders roll up into the
/// album folder; tracks without one are grouped by folder and album title. A
/// folder is an album folder only when every track under it belongs to that
/// one album, and it isn't `root` or one of `artist_folders`.
pub fn album_folders<'a>(
    root: &Path,
    artist_folders: &BTreeSet<PathBuf>,
    tracks: &'a [(PathBuf, TrackMetadata)],
) -> AlbumFolders<'a> {
    let key = |path: &Path, meta: &TrackMetadata| match meta.musicbrainz_release_id.as_deref() {
        Some(id) if !id.trim().is_empty() => format!("release:{}", id.to_ascii_lowercase()),
        _ => format!(
            "folder:{}\u{0}{}",
            path.parent().unwrap_or(root).display(),
            meta.album_name.as_deref().unwrap_or_default()
        ),
    };
    let mut albums: BTreeMap<String, (Option<PathBuf>, Vec<&TrackMetadata>)> = BTreeMap::new();
    // Every album with a track under each folder, to tell album folders from
    // mixed ones.
    let mut under: BTreeMap<&Path, BTreeSet<String>> = BTreeMap::new();
    for (path, meta) in tracks {
        let key = key(path, meta);
        let parent = path.parent().unwrap_or(root);
        for dir in parent.ancestors().take_while(|dir| dir.starts_with(root)) {
            under.entry(dir).or_default().insert(key.clone());
        }
        let entry = albums.entry(key).or_insert((None, Vec::new()));
        entry.0 = Some(match entry.0.take() {
            None => parent.to_path_buf(),
            Some(common) => common_ancestor(&common, parent),
        });
        entry.1.push(meta);
    }
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    for (key, (folder, metas)) in albums {
        let Some(folder) = folder else { continue };
        if folder == root || !folder.starts_with(root) || artist_folders.contains(&folder) {
            skipped.push((folder, AlbumSkip::NotAnAlbumFolder));
        } else if under
            .get(folder.as_path())
            .is_some_and(|keys| keys.len() > 1 || !keys.contains(&key))
        {
            skipped.push((folder, AlbumSkip::Mixed));
        } else {
            found.push((folder, metas));
        }
    }
    skipped.sort();
    skipped.dedup();
    (found, skipped)
}

fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.ancestors()
        .find(|dir| b.starts_with(dir))
        .map_or_default(Path::to_path_buf)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    const ID: &str = "c24da87b-312a-4bb9-8dbc-1461af2788c9";

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gatefold-nfo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_a_new_file_with_an_escaped_name() {
        let dir = dir("new");
        assert_eq!(
            write_artist(&dir, "AC/DC & Friends <x>", ID, false).unwrap(),
            Outcome::Wrote
        );
        let xml = fs::read_to_string(dir.join(ARTIST_FILE)).unwrap();
        assert!(xml.contains("<title>AC/DC &amp; Friends &lt;x&gt;</title>"));
        assert!(xml.contains(&format!("<musicbrainzartistid>{ID}</musicbrainzartistid>")));
        assert!(!dir.join(".artist.nfo.part").exists());
        assert_eq!(
            write_artist(&dir, "AC/DC & Friends <x>", ID, false).unwrap(),
            Outcome::Already
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn completes_an_existing_file_and_keeps_the_rest() {
        let dir = dir("complete");
        let bio = "<?xml version=\"1.0\"?>\n<artist>\n  <biography>Porto Alegre &amp; more.\n\n## Style</biography>\n</artist>\n";
        fs::write(dir.join(ARTIST_FILE), bio).unwrap();
        assert_eq!(
            write_artist(&dir, "Wonkavision", ID, false).unwrap(),
            Outcome::Completed
        );
        let xml = fs::read_to_string(dir.join(ARTIST_FILE)).unwrap();
        assert_eq!(
            xml,
            format!(
                "<?xml version=\"1.0\"?>\n<artist>\n  <title>Wonkavision</title>\n  <musicbrainzartistid>{ID}</musicbrainzartistid>\n  <biography>Porto Alegre &amp; more.\n\n## Style</biography>\n</artist>\n"
            )
        );
        assert_eq!(
            write_artist(&dir, "Wonkavision", ID, false).unwrap(),
            Outcome::Already
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adds_only_what_is_missing_and_never_changes_a_value() {
        let dir = dir("partial");
        let path = dir.join(ARTIST_FILE);
        fs::write(
            &path,
            "<Artist type=\"x\">\r\n<Title>Own Name</Title>\r\n</Artist>",
        )
        .unwrap();
        assert_eq!(
            write_artist(&dir, "Tagged", ID, false).unwrap(),
            Outcome::Completed
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!(
                "<Artist type=\"x\">\r\n  <musicbrainzartistid>{ID}</musicbrainzartistid>\r\n<Title>Own Name</Title>\r\n</Artist>"
            )
        );

        let other = "<artist><musicbrainzartistid>ffffffff-312a-4bb9-8dbc-1461af2788c9</musicbrainzartistid></artist>";
        fs::write(&path, other).unwrap();
        assert_eq!(
            write_artist(&dir, "Tagged", ID, false).unwrap(),
            Outcome::Differs("ffffffff-312a-4bb9-8dbc-1461af2788c9".into())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), other);

        fs::write(&path, "<album><title>x</title></album>").unwrap();
        assert_eq!(
            write_artist(&dir, "Tagged", ID, false).unwrap(),
            Outcome::WrongRoot
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "<album><title>x</title></album>"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_shared_compilation_and_misnamed_folders() {
        assert_eq!(
            artist_title("Bodyjar", None, "Bodyjar", 2),
            Err(Skip::Shared)
        );
        for name in ["Various Artists", "Soundtracks", "VA", "O.S.T."] {
            assert_eq!(
                artist_title(name, None, name, 1),
                Err(Skip::Compilation),
                "{name}"
            );
        }
        assert_eq!(
            artist_title("A & B", Some("A"), "A & B", 1),
            Err(Skip::OtherName("A".into()))
        );
        assert_eq!(
            artist_title("A", None, "A & B", 1),
            Err(Skip::OtherName("A & B".into()))
        );
        assert_eq!(
            artist_title("Bad Cop\u{F022}Bad Cop", Some("Bad Cop/Bad Cop"), "x", 1),
            Ok("Bad Cop/Bad Cop")
        );
        assert_eq!(
            artist_title("Bad Cop:Bad Cop", Some("Bad Cop/Bad Cop"), "x", 1),
            Ok("Bad Cop/Bad Cop")
        );
        assert_eq!(artist_title("Bodyjar", None, "Bodyjar", 1), Ok("Bodyjar"));
    }

    #[test]
    fn names_the_artist_not_a_joint_credit() {
        assert_eq!(
            artist_title("Joey Cape", Some("Joey Cape"), "Joey Cape & Tony Sly", 1),
            Ok("Joey Cape")
        );
    }

    #[test]
    fn a_dry_run_says_what_it_would_do_and_writes_nothing() {
        let dir = dir("dry");
        let path = dir.join(ARTIST_FILE);
        assert_eq!(
            write_artist(&dir, "Wonkavision", ID, true).unwrap(),
            Outcome::Wrote
        );
        assert!(!path.exists());
        assert!(!dir.join(".artist.nfo.part").exists());

        fs::write(&path, "<artist><biography>x</biography></artist>").unwrap();
        assert_eq!(
            write_artist(&dir, "Wonkavision", ID, true).unwrap(),
            Outcome::Completed
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "<artist><biography>x</biography></artist>"
        );
        write_artist(&dir, "Wonkavision", ID, false).unwrap();
        assert_eq!(
            write_artist(&dir, "Wonkavision", ID, true).unwrap(),
            Outcome::Already
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn track(path: &str, release: Option<&str>, number: Option<u32>) -> TrackMetadata {
        TrackMetadata {
            version: crate::metadata::CURRENT_VERSION,
            fingerprint: None,
            path: path.to_string(),
            track_name: Some(format!("Song {}", number.unwrap_or(0))),
            artist_name: Some("Bad Religion".into()),
            album_name: Some("The New America".into()),
            album_artist_name: Some("Bad Religion".into()),
            duration: Some(185),
            has_lyrics_tag: false,
            musicbrainz_release_id: release.map(String::from),
            musicbrainz_release_group_id: None,
            musicbrainz_album_artist_id: None,
            has_cover_art: false,
            barcode: None,
            release: crate::metadata::ReleaseTags {
                track_number: number,
                disc_number: Some(1),
                ..Default::default()
            },
        }
    }

    #[test]
    fn an_album_takes_the_common_values_and_orders_its_tracks() {
        let mut tracks: Vec<TrackMetadata> = (1..=10)
            .rev()
            .map(|n| track(&format!("/m/A/X/{n:02}.flac"), Some(ID), Some(n)))
            .collect();
        for t in &mut tracks {
            t.release.genres = vec!["Punk".into()];
            t.release.date = Some("2000-05-09".into());
            t.release.original_date = Some("1999".into());
            t.release.labels = vec!["Atlantic & Co".into()];
        }
        tracks[0].release.genres = vec!["Odd".into()];
        // A "feat." credit on a single artist's album stays as tagged.
        tracks[1].artist_name = Some("Bad Religion feat. Guest".into());
        tracks[1].release.artists = vec!["Bad Religion".into(), "Guest".into()];
        tracks[2].release.track_number = None;
        tracks[2].path = "/m/A/X/bonus.flac".into();
        tracks[3].release.disc_number = Some(2);
        tracks[3].release.track_number = Some(1);
        tracks[4].musicbrainz_release_id = None;
        let refs: Vec<&TrackMetadata> = tracks.iter().collect();
        let album = album_from(&refs);

        assert_eq!(
            album.genres,
            ["Punk", "Odd"],
            "every genre, most common first"
        );
        assert_eq!(album.year.as_deref(), Some("1999"), "original year first");
        assert_eq!(album.date.as_deref(), Some("2000-05-09"));
        assert_eq!(
            album.artists,
            ["Bad Religion"],
            "the credit when no ALBUMARTISTS"
        );
        assert_eq!(album.runtime, Some(31), "10 × 3:05, rounded");
        assert_eq!(album.total_discs, Some(2));
        let order: Vec<(Option<u32>, Option<u32>)> =
            album.tracks.iter().map(|t| (t.disc, t.position)).collect();
        assert_eq!(order.first(), Some(&(Some(1), Some(1))));
        assert_eq!(
            order[order.len() - 2],
            (Some(2), Some(1)),
            "disc 2 after disc 1"
        );
        assert_eq!(order.last(), Some(&(Some(1), None)), "unnumbered last");
        let guest = album
            .tracks
            .iter()
            .filter(|t| !t.artists.is_empty())
            .count();
        assert_eq!(guest, 1, "a track artist only when it differs");

        let dir = dir("album");
        assert_eq!(write_album(&dir, &album, false).unwrap(), Outcome::Wrote);
        let xml = fs::read_to_string(dir.join(ALBUM_FILE)).unwrap();
        assert!(xml.starts_with(HEADER));
        assert!(xml.contains("  <title>The New America</title>\n"));
        assert!(xml.contains(&format!(
            "  <musicbrainzalbumid>{ID}</musicbrainzalbumid>\n"
        )));
        assert!(xml.contains("  <label>Atlantic &amp; Co</label>\n  <studio>"));
        assert!(xml.contains("  <track>\n    <disc>1</disc>\n    <position>1</position>\n"));
        assert!(xml.contains("    <duration>3:05</duration>\n"));
        assert!(xml.contains("    <artist>Bad Religion feat. Guest</artist>\n"));
        assert!(xml.contains(
            "  <artist>Bad Religion</artist>\n  <albumartist>Bad Religion</albumartist>\n"
        ));
        assert!(
            !xml.contains("musicbrainztrackid"),
            "no ids the tags don't have"
        );
        assert_eq!(xml.matches("<track>").count(), 10);
        assert!(xml.ends_with("  </track>\n</album>\n"));
        assert_eq!(write_album(&dir, &album, false).unwrap(), Outcome::Already);
        fs::remove_dir_all(&dir).unwrap();
    }

    pub(crate) const YELLOWCARD: &str = "3630fff3-52fc-4e97-ab01-d68fd88e4135";
    pub(crate) const HAMMOCK: &str = "5b9d784c-33eb-4a78-a976-c97b672370fa";

    /// A track of "A Hopeful Sign" by Yellowcard + Hammock, tagged as Picard
    /// does: one `ARTIST`/`ALBUMARTIST` string, each artist and id separately.
    pub(crate) fn joint_track() -> TrackMetadata {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/joint-credit.flac");
        crate::metadata::extract(&path).unwrap()
    }

    #[test]
    fn the_joint_fixture_reads_each_artist_and_id() {
        let t = joint_track();
        assert_eq!(t.album_artist_name.as_deref(), Some("Yellowcard + Hammock"));
        assert_eq!(t.release.album_artist_values, ["Yellowcard + Hammock"]);
        assert_eq!(t.release.album_artists, ["Yellowcard", "Hammock"]);
        assert_eq!(t.release.album_artist_ids, [YELLOWCARD, HAMMOCK]);
        assert_eq!(t.release.artists, ["Yellowcard", "Hammock"]);
        assert_eq!(t.release.artist_ids, [YELLOWCARD, HAMMOCK]);
    }

    #[test]
    fn a_joint_release_names_each_artist_next_to_its_own_id() {
        let first = joint_track();
        let mut second = joint_track();
        second.path = "/m/x/02.flac".into();
        second.release.track_number = Some(2);
        let album = album_from(&[&first, &second]);
        assert_eq!(album.artists, ["Yellowcard", "Hammock"]);
        assert_eq!(album.artist_ids, [YELLOWCARD, HAMMOCK]);
        assert!(album.tracks.iter().all(|t| t.artists.is_empty()));

        let dir = dir("album-joint");
        write_album(&dir, &album, false).unwrap();
        let xml = fs::read_to_string(dir.join(ALBUM_FILE)).unwrap();
        assert!(xml.contains(
            "  <artist>Yellowcard</artist>\n  <artist>Hammock</artist>\n  <albumartist>Yellowcard</albumartist>\n  <albumartist>Hammock</albumartist>\n"
        ));
        assert!(xml.contains(&format!(
            "  <musicbrainzalbumartistid>{YELLOWCARD}</musicbrainzalbumartistid>\n  <musicbrainzalbumartistid>{HAMMOCK}</musicbrainzalbumartistid>\n"
        )));
        assert!(!xml.contains("Yellowcard + Hammock"), "{xml}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_track_on_a_joint_release_lists_each_of_its_artists() {
        let mut t = joint_track();
        t.artist_name = Some("Yellowcard + Hammock feat. Guest".into());
        t.release.artists = vec!["Yellowcard".into(), "Hammock".into(), "Guest".into()];
        let album = album_from(&[&t]);
        assert_eq!(album.tracks[0].artists, ["Yellowcard", "Hammock", "Guest"]);
    }

    #[test]
    fn a_joint_credit_with_no_one_artist_gets_no_id() {
        let mut t = joint_track();
        t.release.album_artists.clear();
        t.release.artists.clear();
        let album = album_from(&[&t]);
        assert_eq!(album.artists, ["Yellowcard + Hammock"]);
        assert!(album.artist_ids.is_empty(), "not Yellowcard's id");
        let xml: Vec<String> = album_elements(&album).into_iter().map(|e| e.xml).collect();
        assert!(!xml.join("\n").contains("musicbrainzalbumartistid"));

        // A lone credit with a lone id is that artist's; a mismatch is no one's.
        let one = |n: &str| vec![n.to_string()];
        assert_eq!(
            credited(Some("Clash"), vec![], one(YELLOWCARD)),
            (one("Clash"), one(YELLOWCARD))
        );
        assert_eq!(
            credited(None, vec!["A".into(), "B".into()], one(YELLOWCARD)),
            (vec!["A".into(), "B".into()], vec![])
        );
    }

    #[test]
    fn a_single_album_artist_value_for_several_ids_is_flagged() {
        let t = joint_track();
        assert_eq!(
            joint_album_artist_tag(&[&t]),
            Some(("Yellowcard + Hammock".into(), 2))
        );
        let mut split = joint_track();
        split.release.album_artist_values = vec!["Yellowcard".into(), "Hammock".into()];
        assert_eq!(joint_album_artist_tag(&[&split]), None);
        assert_eq!(
            joint_album_artist_tag(&[&track("/m/A/X/01.flac", Some(ID), Some(1))]),
            None
        );
    }

    #[test]
    fn an_existing_album_nfo_is_only_completed() {
        let dir = dir("album-complete");
        let path = dir.join(ALBUM_FILE);
        let album = album_from(&[&track("/m/A/X/01.flac", Some(ID), Some(1))]);

        // Its own tracks stay; a <title> inside a track isn't the album's.
        let mine = "<album>\n  <track><title>Mine</title></track>\n</album>\n";
        fs::write(&path, mine).unwrap();
        assert_eq!(write_album(&dir, &album, true).unwrap(), Outcome::Completed);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            mine,
            "a dry run writes nothing"
        );
        assert_eq!(
            write_album(&dir, &album, false).unwrap(),
            Outcome::Completed
        );
        let xml = fs::read_to_string(&path).unwrap();
        assert!(xml.starts_with("<album>\n  <title>The New America</title>\n"));
        assert_eq!(
            xml.matches("<track>").count(),
            1,
            "no tracks added to a list"
        );
        assert!(xml.contains("<track><title>Mine</title></track>"));

        let other = "<album><musicbrainzalbumid>ffffffff-312a-4bb9-8dbc-1461af2788c9</musicbrainzalbumid></album>";
        fs::write(&path, other).unwrap();
        assert!(matches!(
            write_album(&dir, &album, false).unwrap(),
            Outcome::Differs(_)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), other);
        fs::write(&path, "<artist/>").unwrap();
        assert_eq!(
            write_album(&dir, &album, false).unwrap(),
            Outcome::WrongRoot
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn album_folders_roll_up_discs_and_skip_mixed_or_loose_tracks() {
        let root = Path::new("/m");
        let artist = PathBuf::from("/m/Clash");
        let tracks: Vec<(PathBuf, TrackMetadata)> = [
            ("/m/Clash/Sandinista/CD 01/01.flac", Some("r1")),
            ("/m/Clash/Sandinista/CD 02/01.flac", Some("r1")),
            ("/m/Clash/London Calling/01.flac", Some("r2")),
            ("/m/Clash/Mixed/01.flac", Some("r3")),
            ("/m/Clash/Mixed/02.flac", Some("r4")),
            ("/m/Clash/loose.flac", Some("r5")),
            ("/m/Clash/Untagged/01.flac", None),
            ("/m/Clash/Untagged/02.flac", None),
            ("/m/VA/Punk Goes 80s/01.flac", Some("r6")),
        ]
        .into_iter()
        .map(|(path, release)| (PathBuf::from(path), track(path, release, Some(1))))
        .collect();
        let (found, skipped) = album_folders(root, &BTreeSet::from([artist.clone()]), &tracks);
        let found: Vec<(String, usize)> = found
            .iter()
            .map(|(dir, metas)| (dir.display().to_string(), metas.len()))
            .collect();
        assert_eq!(
            found,
            [
                ("/m/Clash/Untagged".to_string(), 2),
                ("/m/Clash/Sandinista".to_string(), 2),
                ("/m/Clash/London Calling".to_string(), 1),
                ("/m/VA/Punk Goes 80s".to_string(), 1),
            ]
        );
        assert_eq!(
            skipped,
            [
                (artist, AlbumSkip::NotAnAlbumFolder),
                (PathBuf::from("/m/Clash/Mixed"), AlbumSkip::Mixed),
            ]
        );
    }
}
