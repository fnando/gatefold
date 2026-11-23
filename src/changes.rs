//! What gatefold changed on disk: one [`Change`] per file written, replaced
//! or marked. Every run that isn't a dry run records them in `gatefold.db`
//! (see [`crate::db`]), so `gatefold changes` (and the web UI) can say what
//! happened, and to which artist.

use crate::db::Db;
use crate::writer;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What kind of file (or marker) a change touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    LyricsTag,
    Lrc,
    CoverEmbedded,
    CoverSidecar,
    CoverVideo,
    ArtistAvatar,
    ArtistBackdrop,
    ArtistVideo,
    ArtistLogo,
    ArtistNfo,
    AlbumNfo,
    Kept,
    Unkept,
    Skipped,
    Unskipped,
    Archived,
    Restored,
    Uploaded,
    FixedCase,
}

/// Every kind, for parsing and for filters.
pub const KINDS: &[Kind] = &[
    Kind::LyricsTag,
    Kind::Lrc,
    Kind::CoverEmbedded,
    Kind::CoverSidecar,
    Kind::CoverVideo,
    Kind::ArtistAvatar,
    Kind::ArtistBackdrop,
    Kind::ArtistVideo,
    Kind::ArtistLogo,
    Kind::ArtistNfo,
    Kind::AlbumNfo,
    Kind::Kept,
    Kind::Unkept,
    Kind::Skipped,
    Kind::Unskipped,
    Kind::Archived,
    Kind::Restored,
    Kind::Uploaded,
    Kind::FixedCase,
];

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::LyricsTag => "lyrics_tag",
            Kind::Lrc => "lrc",
            Kind::CoverEmbedded => "cover_embedded",
            Kind::CoverSidecar => "cover_sidecar",
            Kind::CoverVideo => "cover_video",
            Kind::ArtistAvatar => "artist_avatar",
            Kind::ArtistBackdrop => "artist_backdrop",
            Kind::ArtistVideo => "artist_video",
            Kind::ArtistLogo => "artist_logo",
            Kind::ArtistNfo => "artist_nfo",
            Kind::AlbumNfo => "album_nfo",
            Kind::Kept => "kept",
            Kind::Unkept => "unkept",
            Kind::Skipped => "skipped",
            Kind::Unskipped => "unskipped",
            Kind::Archived => "archived",
            Kind::Restored => "restored",
            Kind::Uploaded => "uploaded",
            Kind::FixedCase => "fixed_case",
        }
    }

    pub fn parse(value: &str) -> Option<Kind> {
        KINDS.iter().copied().find(|kind| kind.as_str() == value)
    }

    /// The filter group a kind belongs to: `lyrics`, `covers`, `artist`,
    /// `nfo` or `files` (markers, uploads, renames).
    pub fn group(self) -> &'static str {
        match self {
            Kind::LyricsTag | Kind::Lrc => "lyrics",
            Kind::CoverEmbedded | Kind::CoverSidecar | Kind::CoverVideo => "covers",
            Kind::ArtistAvatar | Kind::ArtistBackdrop | Kind::ArtistVideo | Kind::ArtistLogo => {
                "artist"
            }
            Kind::ArtistNfo | Kind::AlbumNfo => "nfo",
            Kind::Kept
            | Kind::Unkept
            | Kind::Skipped
            | Kind::Unskipped
            | Kind::Archived
            | Kind::Restored
            | Kind::Uploaded
            | Kind::FixedCase => "files",
        }
    }
}

/// Whether the file is new or took the place of another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Created,
    Replaced,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Created => "created",
            Action::Replaced => "replaced",
        }
    }

    pub fn parse(value: &str) -> Option<Action> {
        match value {
            "created" => Some(Action::Created),
            "replaced" => Some(Action::Replaced),
            _ => None,
        }
    }

    /// The action for a write, `None` when nothing was written.
    pub fn of(write: writer::Write) -> Option<Action> {
        match write {
            writer::Write::Created => Some(Action::Created),
            writer::Write::Replaced => Some(Action::Replaced),
            writer::Write::Skipped => None,
        }
    }
}

/// One file gatefold wrote, replaced or marked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub kind: Kind,
    pub action: Action,
    /// The (album) artist it belongs to.
    pub artist: Option<String>,
    /// The artist folder, which the change links back to.
    pub artist_dir: Option<PathBuf>,
    pub album_dir: Option<PathBuf>,
    /// The file written (a track, `cover.jpg`, `folder.jpg`, `artist.nfo`, …).
    pub file: PathBuf,
    /// Where it came from (`lrclib`, `apple music`, `cover art archive`, …).
    pub source: Option<String>,
}

impl Change {
    /// What happened, in words: "Replaced plain lyrics with synced lyrics".
    pub fn describe(&self) -> &'static str {
        describe(self.kind, self.action)
    }
}

/// What a change of `kind` did, in words.
pub fn describe(kind: Kind, action: Action) -> &'static str {
    let created = action == Action::Created;
    match kind {
        Kind::LyricsTag if created => "Embedded lyrics",
        Kind::LyricsTag => "Replaced embedded lyrics",
        Kind::Lrc if created => "Added synced lyrics",
        // `write_lrc` only replaces an `.lrc` with no timestamps.
        Kind::Lrc => "Replaced plain lyrics with synced lyrics",
        Kind::CoverEmbedded if created => "Embedded cover",
        Kind::CoverEmbedded => "Replaced embedded cover",
        Kind::CoverSidecar if created => "Added cover",
        Kind::CoverSidecar => "Replaced cover",
        Kind::CoverVideo if created => "Added animated cover",
        Kind::CoverVideo => "Replaced animated cover",
        Kind::ArtistAvatar if created => "Added artist photo",
        Kind::ArtistAvatar => "Updated artist photo",
        Kind::ArtistBackdrop if created => "Added backdrop",
        Kind::ArtistBackdrop => "Updated backdrop",
        Kind::ArtistVideo if created => "Added motion backdrop",
        Kind::ArtistVideo => "Updated motion backdrop",
        Kind::ArtistLogo if created => "Added artist logo",
        Kind::ArtistLogo => "Updated artist logo",
        Kind::ArtistNfo if created => "Wrote artist.nfo",
        Kind::ArtistNfo => "Completed artist.nfo",
        Kind::AlbumNfo if created => "Wrote album.nfo",
        Kind::AlbumNfo => "Completed album.nfo",
        Kind::Kept => "Kept",
        Kind::Unkept => "Stopped keeping",
        Kind::Skipped => "Skipped",
        Kind::Unskipped => "Took back a skip",
        Kind::Archived => "Archived",
        Kind::Restored => "Restored",
        Kind::Uploaded => "Uploaded",
        Kind::FixedCase => "Fixed the file name's case",
    }
}

/// A change to `path` itself (a marker, an archive move, an upload): the
/// album folder of a cover, the artist folder of an artist file, each named
/// after its folder.
pub fn for_file(kind: Kind, action: Action, path: &std::path::Path) -> Change {
    use crate::skip::{self, Managed};
    let name = |dir: &std::path::Path| dir.file_name().map(|n| n.to_string_lossy().into_owned());
    let parent = |dir: &std::path::Path| dir.parent().map(std::path::Path::to_path_buf);
    let (artist_dir, album_dir) = if path.is_dir() {
        // A folder with tracks is an album folder; any other, an artist folder.
        if crate::scanner::dir_has_audio(path) {
            (parent(path), Some(path.to_path_buf()))
        } else {
            (Some(path.to_path_buf()), None)
        }
    } else {
        match skip::classify(path) {
            Some((Managed::Cover, album)) => (parent(&album), Some(album)),
            Some((Managed::Artist(_), artist)) => (Some(artist), None),
            None => (parent(path), None),
        }
    };
    Change {
        kind,
        action,
        artist: artist_dir.as_deref().and_then(name),
        artist_dir,
        album_dir,
        file: path.to_path_buf(),
        source: None,
    }
}

/// Records a run's changes in the database. Failing to record never fails the
/// run: the first error is reported, the rest are dropped quietly.
pub struct Recorder {
    db: Mutex<Db>,
    run_id: Option<i64>,
    warned: AtomicBool,
}

impl Recorder {
    pub fn new(db: Db, run_id: Option<i64>) -> Self {
        Self {
            db: Mutex::new(db),
            run_id,
            warned: AtomicBool::new(false),
        }
    }

    pub fn record(&self, change: &Change) {
        let result = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))
            .and_then(|db| db.insert_change(self.run_id, crate::db::now(), change));
        if let Err(err) = result
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!("warning: couldn't record a change in gatefold.db: {err:#}");
        }
    }

    /// Keep a log line in the run's history.
    pub fn line(&self, kind: &str, text: &str) {
        let Some(run) = self.run_id else {
            return;
        };
        let result = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))
            .and_then(|db| db.insert_line(run, kind, text));
        if let Err(err) = result
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!("warning: couldn't record the run's log in gatefold.db: {err:#}");
        }
    }

    /// The database, for the run's own bookkeeping (start and finish).
    pub fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn run_id(&self) -> Option<i64> {
        self.run_id
    }
}

/// When a change happened, in local time: `2026-11-23 14:02`.
pub fn local_time(at: i64) -> String {
    jiff::Timestamp::from_second(at).map_or_else(
        |_| at.to_string(),
        |ts| {
            ts.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// The file a change touched, as short as it can be said: relative to its
/// artist folder (`Battle Scars/cover.mp4`, `logo.png`), else its album
/// folder's parent, else in full.
pub fn short_file(change: &Change) -> String {
    // A folder-wide keep: the artist folder itself, by name.
    if Some(&change.file) == change.artist_dir.as_ref() {
        return change.file.file_name().map_or_else(
            || change.file.display().to_string(),
            |n| format!("{}/", n.to_string_lossy()),
        );
    }
    let bases = [
        change.artist_dir.as_deref(),
        change
            .album_dir
            .as_deref()
            .and_then(std::path::Path::parent),
    ];
    bases
        .into_iter()
        .flatten()
        .find_map(|base| change.file.strip_prefix(base).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .unwrap_or(&change.file)
        .display()
        .to_string()
}

/// `gatefold changes`: one line per change, under a heading per day.
pub fn render(rows: &[crate::db::ChangeRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no changes recorded yet".to_string()];
    }
    let mut lines = Vec::new();
    let mut day = String::new();
    for row in rows {
        let when = local_time(row.at);
        let (date, time) = when.split_once(' ').unwrap_or((when.as_str(), ""));
        if date != day {
            if !day.is_empty() {
                lines.push(String::new());
            }
            lines.push(date.to_string());
            day = date.to_string();
        }
        let change = &row.change;
        let artist = change.artist.as_deref().unwrap_or("—");
        let source = change
            .source
            .as_deref()
            .map_or_else(String::new, |s| format!(" ({s})"));
        lines.push(format!(
            "  {time}  {artist} · {}: {}{source}",
            change.describe(),
            short_file(change)
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_named_from_their_artist_folder() {
        let change = Change {
            kind: Kind::CoverVideo,
            action: Action::Created,
            artist: Some("The Interrupters".into()),
            artist_dir: Some(PathBuf::from("/m/The Interrupters")),
            album_dir: Some(PathBuf::from("/m/The Interrupters/Battle Scars")),
            file: PathBuf::from("/m/The Interrupters/Battle Scars/cover.mp4"),
            source: Some("apple music".into()),
        };
        assert_eq!(short_file(&change), "Battle Scars/cover.mp4");
        let loose = Change {
            artist_dir: None,
            ..change.clone()
        };
        assert_eq!(short_file(&loose), "Battle Scars/cover.mp4");
        let folder = Change {
            file: PathBuf::from("/m/The Interrupters"),
            ..change.clone()
        };
        assert_eq!(short_file(&folder), "The Interrupters/");
        let rows = [crate::db::ChangeRow {
            id: 1,
            run_id: None,
            at: 0,
            change,
        }];
        let lines = render(&rows);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].ends_with(
                "The Interrupters · Added animated cover: Battle Scars/cover.mp4 (apple music)"
            ),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn kinds_round_trip() {
        for kind in KINDS {
            assert_eq!(Kind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(Kind::parse("nope"), None);
    }

    #[test]
    fn describes_the_lyrics_upgrade() {
        assert_eq!(
            describe(Kind::Lrc, Action::Replaced),
            "Replaced plain lyrics with synced lyrics"
        );
        assert_eq!(
            describe(Kind::CoverVideo, Action::Created),
            "Added animated cover"
        );
        assert_eq!(
            describe(Kind::ArtistLogo, Action::Replaced),
            "Updated artist logo"
        );
    }
}
