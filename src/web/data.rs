//! What the pages show, read from `gatefold.db` and the daemon.

use crate::changes::{self, Kind};
use crate::daemon::Daemon;
use crate::db::{ChangeQuery, ChangeRow, Db, RunRow};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::Path;

/// The daemon at a glance.
#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_excessive_bools)] // independent states, shown as is
pub struct Status {
    /// A run is going on in some lane.
    pub running: bool,
    pub paused: bool,
    pub jobs: Vec<JobView>,
    /// Runs asked from the UI (not a job): one going on, and how many wait.
    pub manual_running: bool,
    pub queued: usize,
    pub libraries: Vec<String>,
    pub runs: Vec<RunView>,
    pub changes: Vec<ChangeGroup>,
    pub problems: Vec<ProblemView>,
}

impl Status {
    /// Some job runs on an interval (so there's a schedule to pause).
    pub fn has_schedules(&self) -> bool {
        self.jobs.iter().any(|job| job.schedule.is_some())
    }
}

/// One of `gatefold run`'s jobs.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub name: String,
    /// "every 5m", or `None`: only when asked.
    pub schedule: Option<String>,
    /// What it does, in short: "new only · nfo · verify".
    pub does: String,
    /// When it runs next ("in 2h", "now").
    pub next: Option<String>,
    pub running: bool,
    pub last: Option<RunView>,
}

/// A job's options, in short.
fn job_does(job: &crate::config::Job) -> String {
    use crate::config::{Only, Replace, Replaceable};
    let mut parts: Vec<String> = Vec::new();
    match job.only {
        Some(Only::Lyrics) => parts.push("lyrics only".into()),
        Some(Only::Covers) => parts.push("covers only".into()),
        Some(Only::ArtistImages) => parts.push("artist images only".into()),
        None => {}
    }
    if job.new_only {
        parts.push("new items only".into());
    }
    if job.miss_only {
        parts.push("missed items only".into());
    }
    match &job.replace {
        Some(Replace::All(true)) => parts.push("replaces everything".into()),
        Some(Replace::Only(what)) if !what.is_empty() => {
            let names: Vec<&str> = what
                .iter()
                .map(|w| match w {
                    Replaceable::Lyrics => "lyrics",
                    Replaceable::Covers => "covers",
                    Replaceable::ArtistImages => "artist images",
                })
                .collect();
            parts.push(format!("replaces {}", names.join(", ")));
        }
        _ => {}
    }
    if job.artist_images == Some(false) {
        parts.push("no artist images".into());
    }
    if job.write_nfo == Some(true) {
        parts.push("nfo".into());
    }
    if job.verify == Some(true) {
        parts.push("verify".into());
    }
    if let Some(after) = &job.retry_missed_after {
        parts.push(format!("retries misses after {after}"));
    }
    if parts.is_empty() {
        "everything".to_string()
    } else {
        parts.join(" · ")
    }
}

pub fn status(daemon: &Daemon, db: &Db) -> anyhow::Result<Status> {
    let rows = db.changes(&ChangeQuery {
        limit: 60,
        ..ChangeQuery::default()
    })?;
    let jobs = daemon
        .jobs
        .iter()
        .map(|job| {
            Ok(JobView {
                name: job.name.clone(),
                schedule: job
                    .interval
                    .map(|i| format!("every {}", crate::cli::format_duration(i))),
                does: job_does(&job.job),
                next: daemon.next_scheduled(&job.name).map(when_next),
                running: daemon.running(&job.name),
                last: db.last_run_of(&job.name)?.as_ref().map(RunView::from),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Status {
        running: crate::events::live().running(),
        paused: daemon.paused(),
        jobs,
        manual_running: daemon.running(crate::daemon::MANUAL),
        queued: daemon.queued(),
        libraries: daemon
            .roots
            .iter()
            .map(|dir| dir.display().to_string())
            .collect(),
        runs: db.runs(8, 0)?.iter().map(RunView::from).collect(),
        changes: group(&rows).into_iter().take(12).collect(),
        problems: problem_views(&daemon.roots, &daemon.problems()),
    })
}

/// A file name in the wrong case.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemView {
    pub text: String,
    pub folder: String,
    pub link: String,
}

pub fn problem_views(
    roots: &[std::path::PathBuf],
    problems: &[crate::casefix::Problem],
) -> Vec<ProblemView> {
    problems
        .iter()
        .map(|problem| {
            let dir = problem.expected.parent().unwrap_or(Path::new("/"));
            let link = if crate::scanner::dir_has_audio(dir) {
                album_link(dir)
            } else {
                artist_link(&dir.display().to_string())
            };
            ProblemView {
                text: problem.describe(),
                folder: super::display_path(roots, dir),
                link,
            }
        })
        .collect()
}

/// A run, for lists and its page.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub id: i64,
    pub trigger: String,
    pub job: Option<String>,
    pub dir: String,
    pub status: String,
    pub dry_run: bool,
    pub started: String,
    pub started_full: String,
    pub duration: Option<String>,
    pub changes: i64,
    pub error: Option<String>,
}

impl From<&RunRow> for RunView {
    fn from(run: &RunRow) -> Self {
        Self {
            id: run.id,
            trigger: run.trigger.clone(),
            job: run.job.clone(),
            dir: run.scope_dir.clone().unwrap_or_default(),
            status: run.status.clone(),
            dry_run: run.dry_run,
            started: ago(run.started_at),
            started_full: changes::local_time(run.started_at),
            duration: run.finished_at.map(|end| {
                let secs = u64::try_from(end - run.started_at).unwrap_or(0);
                compact(secs)
            }),
            changes: run.changes,
            error: run.error.clone(),
        }
    }
}

impl RunView {
    pub fn trigger_label(&self) -> String {
        if let Some(job) = &self.job {
            return match self.trigger.as_str() {
                "schedule" => format!("{job}, scheduled"),
                _ => format!("{job}, asked for"),
            };
        }
        match self.trigger.as_str() {
            "schedule" => "Scheduled",
            "manual" => "From the web UI",
            "cli" => "From the command line",
            other => other,
        }
        .to_string()
    }
}

/// One file in a group of changes.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeItem {
    pub text: String,
    pub file: String,
    pub source: Option<String>,
    pub group: String,
    pub kind: Kind,
    pub action: changes::Action,
    /// What the change was to: a file, or a folder itself (a folder-wide keep).
    pub target: Target,
}

/// What a change was to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Target {
    File,
    ArtistFolder,
    /// An album folder, by name.
    AlbumFolder(String),
}

impl ChangeItem {
    /// Markers, uploads and moves happen to a file one after another: they
    /// read as a sequence (`kept → not kept → archived`), not as tags.
    fn transition(&self) -> bool {
        self.kind.group() == "files"
    }

    /// Changes to a track (its tags, its `.lrc`) share one row, named after it.
    fn track_change(&self) -> bool {
        matches!(self.kind, Kind::LyricsTag | Kind::Lrc | Kind::CoverEmbedded)
    }

    /// What happened, as a short tag next to the file.
    fn tag(&self) -> String {
        let created = self.action == changes::Action::Created;
        match self.kind {
            Kind::LyricsTag if created => "lyrics embedded".into(),
            Kind::LyricsTag => "lyrics replaced".into(),
            Kind::Lrc if created => "synced .lrc".into(),
            Kind::Lrc => "plain → synced .lrc".into(),
            Kind::CoverEmbedded if created => "cover embedded".into(),
            Kind::CoverEmbedded => "cover replaced".into(),
            Kind::Kept => "kept".into(),
            Kind::Unkept => "not kept".into(),
            Kind::Skipped => "skipped".into(),
            Kind::Unskipped => "not skipped".into(),
            Kind::Archived => "archived".into(),
            Kind::Restored => "restored".into(),
            Kind::Uploaded => "uploaded".into(),
            Kind::FixedCase => "renamed".into(),
            _ => self.text.to_lowercase(),
        }
    }
}

/// A file (or a track, for its tags and `.lrc`) and what happened to it.
#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub name: String,
    pub tags: Vec<String>,
    /// The tags are a sequence of states, oldest first.
    pub transition: bool,
}

/// The files of one folder in a group of changes.
#[derive(Debug, Clone, Serialize)]
pub struct FolderFiles {
    /// The album folder, relative to the artist's; `None` for the artist's own.
    pub folder: Option<String>,
    pub files: Vec<FileRow>,
}

/// Changes one run made for one artist, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeGroup {
    pub artist: String,
    pub artist_dir: Option<String>,
    pub run_id: Option<i64>,
    pub at: String,
    pub at_full: String,
    pub items: Vec<ChangeItem>,
}

impl ChangeGroup {
    /// "Embedded cover ×12 · Added animated cover".
    pub fn summary(&self) -> String {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for item in &self.items {
            match counts.iter_mut().find(|(text, _)| *text == item.text) {
                Some((_, n)) => *n += 1,
                None => counts.push((&item.text, 1)),
            }
        }
        counts
            .iter()
            .map(|(text, n)| {
                if *n > 1 {
                    format!("{text} ×{n}")
                } else {
                    (*text).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Where the changes came from (`lrclib`, `apple music`), once each.
    pub fn sources(&self) -> String {
        let mut sources: Vec<&str> = Vec::new();
        for source in self.items.iter().filter_map(|i| i.source.as_deref()) {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        sources.join(", ")
    }

    /// The files, by folder, oldest first (as they were written), a track's
    /// changes on one row.
    pub fn folders(&self) -> Vec<FolderFiles> {
        let mut folders: Vec<FolderFiles> = Vec::new();
        for item in self.items.iter().rev() {
            let path = Path::new(&item.file);
            let (folder, name) = match &item.target {
                Target::ArtistFolder => (None, "This folder".to_string()),
                Target::AlbumFolder(album) => (Some(album.clone()), "This folder".to_string()),
                Target::File => (
                    path.parent()
                        .map(|p| p.display().to_string())
                        .filter(|p| !p.is_empty()),
                    if item.track_change() {
                        path.file_stem()
                    } else {
                        path.file_name()
                    }
                    .map_or_else(|| item.file.clone(), |n| n.to_string_lossy().into_owned()),
                ),
            };
            let index = if let Some(index) = folders.iter().position(|f| f.folder == folder) {
                index
            } else {
                folders.push(FolderFiles {
                    folder,
                    files: Vec::new(),
                });
                folders.len() - 1
            };
            let files = &mut folders[index].files;
            let tag = item.tag();
            let transition = item.transition();
            match files
                .iter_mut()
                .find(|f| f.name == name && f.transition == transition)
            {
                // A sequence keeps every step (kept → not kept → kept); tags
                // are listed once.
                Some(file) if transition || !file.tags.contains(&tag) => file.tags.push(tag),
                Some(_) => {}
                None => files.push(FileRow {
                    name,
                    tags: vec![tag],
                    transition,
                }),
            }
        }
        for folder in &mut folders {
            folder
                .files
                .sort_by(|a, b| natord::compare_ignore_case(&a.name, &b.name));
        }
        folders
    }

    /// The groups of its items (`lyrics`, `covers`, …), for the colour dots.
    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = Vec::new();
        for item in &self.items {
            if !groups.contains(&item.group) {
                groups.push(item.group.clone());
            }
        }
        groups
    }
}

/// Changes in a row with the same run and artist become one group.
pub fn group(rows: &[ChangeRow]) -> Vec<ChangeGroup> {
    let mut groups: Vec<ChangeGroup> = Vec::new();
    for row in rows {
        let change = &row.change;
        let dir = change
            .artist_dir
            .as_ref()
            .map(|dir| dir.display().to_string());
        let artist = change
            .artist
            .clone()
            .unwrap_or_else(|| "Unknown artist".to_string());
        let item = ChangeItem {
            text: change.describe().to_string(),
            file: changes::short_file(change),
            source: change.source.clone(),
            group: change.kind.group().to_string(),
            kind: change.kind,
            action: change.action,
            target: if Some(&change.file) == change.artist_dir.as_ref() {
                Target::ArtistFolder
            } else if Some(&change.file) == change.album_dir.as_ref() {
                Target::AlbumFolder(
                    change
                        .file
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                )
            } else {
                Target::File
            },
        };
        match groups.last_mut() {
            Some(last)
                if last.run_id == row.run_id && last.artist == artist && last.artist_dir == dir =>
            {
                last.items.push(item);
            }
            _ => groups.push(ChangeGroup {
                artist,
                artist_dir: dir,
                run_id: row.run_id,
                at: ago(row.at),
                at_full: changes::local_time(row.at),
                items: vec![item],
            }),
        }
    }
    groups
}

/// The kinds behind a filter chip.
pub fn kinds_of(group: &str) -> Vec<Kind> {
    changes::KINDS
        .iter()
        .copied()
        .filter(|kind| kind.group() == group)
        .collect()
}

/// The filter chips of the changes list.
pub const GROUPS: &[(&str, &str)] = &[
    ("lyrics", "Lyrics"),
    ("covers", "Covers"),
    ("artist", "Artist images"),
    ("nfo", "nfo"),
    ("files", "Kept, skipped & more"),
];

/// What a filter group is, in words (for a dot's caption).
pub fn group_label(key: &str) -> &'static str {
    match key {
        "lyrics" => "Lyrics",
        "covers" => "Covers",
        "artist" => "Artist images",
        "nfo" => "nfo files",
        "files" => "Kept, skipped, archived, restored or uploaded",
        _ => "Other changes",
    }
}

/// A run's counts, from its stored summary, as stat tiles.
#[derive(Debug, Clone, Serialize)]
pub struct Stat {
    pub label: &'static str,
    pub value: u64,
    pub class: &'static str,
}

pub fn stats(summary_json: Option<&str>) -> Vec<Stat> {
    let Some(summary) =
        summary_json.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
    else {
        return Vec::new();
    };
    let get = |key: &str| {
        summary
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let sum = |keys: &[&str]| keys.iter().map(|k| get(k)).sum::<u64>();
    vec![
        Stat {
            label: "lyrics",
            value: sum(&["wrote_tag", "replaced_tag", "wrote_lrc", "replaced_lrc"]),
            class: "s-downloaded",
        },
        Stat {
            label: "covers",
            value: sum(&[
                "embedded_cover",
                "replaced_cover",
                "wrote_cover_sidecar",
                "replaced_cover_sidecar",
                "wrote_cover_video",
                "replaced_cover_video",
            ]),
            class: "s-downloaded",
        },
        Stat {
            label: "artists",
            value: sum(&["artist_wrote", "artist_replaced"]),
            class: "s-downloaded",
        },
        Stat {
            label: "not found",
            value: sum(&[
                "lyrics_not_found",
                "cover_not_found",
                "cover_video_not_found",
            ]),
            class: "s-skipped",
        },
        Stat {
            label: "errors",
            value: sum(&[
                "unreadable",
                "lyrics_errors",
                "cover_errors",
                "cover_video_errors",
                "artist_errors",
            ]),
            class: "s-failed",
        },
    ]
}

/// "just now", "5m ago", "3h ago", "2d ago", else the date.
pub fn ago(at: i64) -> String {
    let secs = crate::db::now() - at;
    match secs {
        ..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        86_400..604_800 => format!("{}d ago", secs / 86_400),
        _ => changes::local_time(at)
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// "now", "in 5m", "in 3h".
fn when_next(at: i64) -> String {
    let secs = at - crate::db::now();
    if secs <= 30 {
        "now".to_string()
    } else {
        format!("in {}", compact(u64::try_from(secs).unwrap_or(0)))
    }
}

/// `45s`, `12m`, `3h 5m`, `2d 4h`.
pub fn compact(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Percent-encode a query value.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The artist page of a folder.
pub fn artist_link(dir: &str) -> String {
    format!("/artist?dir={}", encode(dir))
}

/// The album page of a folder.
pub fn album_link(dir: &Path) -> String {
    format!("/album?dir={}", encode(&dir.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Action, Change};
    use std::path::PathBuf;

    fn row(id: i64, run: i64, artist: &str, kind: Kind) -> ChangeRow {
        ChangeRow {
            id,
            run_id: Some(run),
            at: crate::db::now(),
            change: Change {
                kind,
                action: Action::Created,
                artist: Some(artist.into()),
                artist_dir: Some(PathBuf::from(format!("/m/{artist}"))),
                album_dir: None,
                file: PathBuf::from(format!("/m/{artist}/x")),
                source: None,
            },
        }
    }

    #[test]
    fn changes_group_by_run_and_artist() {
        let rows = [
            row(5, 2, "AFI", Kind::CoverEmbedded),
            row(4, 2, "AFI", Kind::CoverEmbedded),
            row(3, 2, "AFI", Kind::CoverVideo),
            row(2, 2, "Paramore", Kind::Lrc),
            row(1, 1, "AFI", Kind::ArtistLogo),
        ];
        let groups = group(&rows);
        assert_eq!(groups.len(), 3);
        assert_eq!(
            groups[0].summary(),
            "Embedded cover ×2 · Added animated cover"
        );
        assert_eq!(groups[0].groups(), ["covers"]);
        assert_eq!(groups[1].artist, "Paramore");
        assert_eq!(groups[2].summary(), "Added artist logo");
    }

    #[test]
    fn markers_read_as_a_sequence() {
        let marker = |id, kind, file: &str| {
            let mut row = row(id, 0, "Paramore", kind);
            row.run_id = None;
            row.change.file = PathBuf::from(file);
            row
        };
        // Newest first, as stored.
        let rows = [
            marker(6, Kind::Restored, "/m/Paramore/folder.jpg"),
            marker(5, Kind::Archived, "/m/Paramore/folder.jpg"),
            marker(4, Kind::Unkept, "/m/Paramore/folder.jpg"),
            marker(3, Kind::Unkept, "/m/Paramore"),
            marker(2, Kind::Kept, "/m/Paramore/folder.jpg"),
            marker(1, Kind::Kept, "/m/Paramore"),
        ];
        let folders = group(&rows)[0].folders();
        assert_eq!(folders.len(), 1);
        assert_eq!(
            folders[0].folder, None,
            "the artist folder's own files have no heading"
        );
        let files: Vec<(&str, Vec<&str>)> = folders[0]
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.tags.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            files,
            [
                (
                    "folder.jpg",
                    vec!["kept", "not kept", "archived", "restored"]
                ),
                ("This folder", vec!["kept", "not kept"]),
            ]
        );
        assert!(folders[0].files.iter().all(|f| f.transition));
    }

    #[test]
    fn a_tracks_changes_share_a_row() {
        let lyrics = |id, kind, file: &str| {
            let mut row = row(id, 1, "Os Mutantes", kind);
            row.change.file = PathBuf::from(format!("/m/Os Mutantes/Tropicália/{file}"));
            row.change.source = Some("lrclib".into());
            row
        };
        let rows = [
            lyrics(4, Kind::LyricsTag, "02 - Baby.flac"),
            lyrics(3, Kind::Lrc, "02 - Baby.lrc"),
            lyrics(2, Kind::LyricsTag, "01 - Miserere Nobis.flac"),
            lyrics(1, Kind::Lrc, "01 - Miserere Nobis.lrc"),
        ];
        let group = &group(&rows)[0];
        assert_eq!(group.sources(), "lrclib");
        let folders = group.folders();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].folder.as_deref(), Some("Tropicália"));
        let files: Vec<(&str, Vec<&str>)> = folders[0]
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.tags.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            files,
            [
                (
                    "01 - Miserere Nobis",
                    vec!["synced .lrc", "lyrics embedded"]
                ),
                ("02 - Baby", vec!["synced .lrc", "lyrics embedded"]),
            ]
        );
    }

    #[test]
    fn durations_and_links() {
        assert_eq!(compact(45), "45s");
        assert_eq!(compact(3 * 3600 + 5 * 60), "3h 5m");
        assert_eq!(compact(2 * 86_400 + 4 * 3600), "2d 4h");
        assert_eq!(
            artist_link("/m/Rock & Roll"),
            "/artist?dir=/m/Rock%20%26%20Roll"
        );
    }

    #[test]
    fn stats_read_the_summary() {
        let stats = stats(Some(
            r#"{"embedded_cover": 3, "wrote_cover_video": 1, "lyrics_errors": 2}"#,
        ));
        assert_eq!(stats[1].value, 4);
        assert_eq!(stats[4].value, 2);
        assert!(super::stats(None).is_empty());
    }
}
