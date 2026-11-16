//! `.nfo` files Jellyfin (and Kodi) read: for now an artist folder's
//! `artist.nfo`, with the artist's name and `MusicBrainz` id so Jellyfin matches
//! the folder to the right artist. An existing file is only ever completed:
//! missing elements are added, everything else is left byte for byte.

use crate::artist::normalize_name;
use crate::paths::write_atomic;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub const ARTIST_FILE: &str = "artist.nfo";

const TITLE: &str = "title";
const MBID: &str = "musicbrainzartistid";

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

/// What [`write_artist`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Wrote,
    /// An existing file got the elements it was missing.
    Completed,
    Already,
    /// The existing file names this other `MusicBrainz` id; left as is.
    Differs(String),
    /// The existing file has no `<artist>` element; left as is.
    NotArtistXml,
}

/// Why an artist folder gets no `artist.nfo`.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    /// Albums of several album artists live in it.
    Shared,
    /// Named like a compilations folder.
    Compilation,
    /// `MusicBrainz` names the artist differently from the folder (a joint
    /// album artist tagged with one artist's id, say).
    OtherName(String),
}

/// Whether artist folder `folder_name` should be skipped: `claims` is how many
/// album artists found it their folder, `mb_name` the `MusicBrainz` artist's name
/// when known.
pub fn skip_reason(folder_name: &str, mb_name: Option<&str>, claims: usize) -> Option<Skip> {
    let folder = normalize_name(folder_name);
    if claims > 1 {
        return Some(Skip::Shared);
    }
    if COMPILATION_NAMES.contains(&folder.as_str()) {
        return Some(Skip::Compilation);
    }
    match mb_name {
        Some(name) if normalize_name(name) != folder => Some(Skip::OtherName(name.to_string())),
        _ => None,
    }
}

/// Write `artist.nfo` in `folder` naming the artist and its `MusicBrainz` id,
/// or complete an existing one with whichever of the two it lacks. On a
/// `dry_run`, says what it would do and writes nothing.
pub fn write_artist(folder: &Path, name: &str, mbid: &str, dry_run: bool) -> Result<Outcome> {
    let path = folder.join(ARTIST_FILE);
    let existing = match fs::read(&path) {
        Ok(bytes) => Some(
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?,
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let Some(existing) = existing else {
        if dry_run {
            return Ok(Outcome::Wrote);
        }
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<artist>\n  {}\n  {}\n</artist>\n",
            element(TITLE, name),
            element(MBID, mbid)
        );
        write_atomic(&path, xml.as_bytes())?;
        return Ok(Outcome::Wrote);
    };
    Ok(match complete(&existing, name, mbid) {
        Completion::Changed(xml) => {
            if !dry_run {
                write_atomic(&path, xml.as_bytes())?;
            }
            Outcome::Completed
        }
        Completion::Already => Outcome::Already,
        Completion::Differs(id) => Outcome::Differs(id),
        Completion::NotArtistXml => Outcome::NotArtistXml,
    })
}

enum Completion {
    Changed(String),
    Already,
    Differs(String),
    NotArtistXml,
}

/// `xml` with the missing elements inserted right after `<artist>`.
fn complete(xml: &str, name: &str, mbid: &str) -> Completion {
    let Some(open_end) = open_tag_end(xml, "artist") else {
        return Completion::NotArtistXml;
    };
    if let Some(id) = value(xml, MBID)
        && !id.eq_ignore_ascii_case(mbid)
    {
        return Completion::Differs(id);
    }
    let newline = if xml.contains("\r\n") { "\r\n" } else { "\n" };
    let mut missing = Vec::new();
    if open_tag_end(xml, TITLE).is_none() {
        missing.push(element(TITLE, name));
    }
    if value(xml, MBID).is_none() {
        missing.push(element(MBID, mbid));
    }
    let indent = format!("{newline}  ");
    let insert = if missing.is_empty() {
        String::new()
    } else {
        format!("{indent}{}", missing.join(&indent))
    };
    if insert.is_empty() {
        return Completion::Already;
    }
    Completion::Changed(format!("{}{insert}{}", &xml[..open_end], &xml[open_end..]))
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

#[cfg(test)]
mod tests {
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
            Outcome::NotArtistXml
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "<album><title>x</title></album>"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_shared_compilation_and_misnamed_folders() {
        assert_eq!(skip_reason("Bodyjar", None, 2), Some(Skip::Shared));
        for name in ["Various Artists", "Soundtracks", "VA", "O.S.T."] {
            assert_eq!(
                skip_reason(name, None, 1),
                Some(Skip::Compilation),
                "{name}"
            );
        }
        assert_eq!(
            skip_reason("A & B", Some("A"), 1),
            Some(Skip::OtherName("A".into()))
        );
        assert_eq!(
            skip_reason("Bad Cop\u{F022}Bad Cop", Some("Bad Cop/Bad Cop"), 1),
            None
        );
        assert_eq!(
            skip_reason("Bad Cop:Bad Cop", Some("Bad Cop/Bad Cop"), 1),
            None
        );
        assert_eq!(skip_reason("Bodyjar", None, 1), None);
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
}
