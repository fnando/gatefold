//! `gatefold.db` in the state dir: every run and the changes it made (see
//! [`crate::changes`]). `SQLite` in WAL mode, so a CLI run and the daemon can
//! write at the same time.

use crate::changes::{Action, Change, Kind};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use rusqlite_migration::{M, Migrations};
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FILE: &str = "gatefold.db";

const MIGRATION_LIST: &[M<'static>] = &[
    M::up(
        "CREATE TABLE runs (
       id INTEGER PRIMARY KEY,
       trigger TEXT NOT NULL,          -- cli, schedule, manual
       scope_dir TEXT,                 -- the folder the run covered
       dry_run INTEGER NOT NULL DEFAULT 0,
       started_at INTEGER NOT NULL,    -- unix seconds
       finished_at INTEGER,
       status TEXT NOT NULL,           -- running, ok, failed, stopped, interrupted
       summary_json TEXT,
       error TEXT
     );
     CREATE TABLE changes (
       id INTEGER PRIMARY KEY,
       run_id INTEGER REFERENCES runs(id) ON DELETE SET NULL,
       at INTEGER NOT NULL,
       kind TEXT NOT NULL,
       action TEXT NOT NULL,
       artist TEXT,
       artist_dir TEXT,
       album_dir TEXT,
       file TEXT NOT NULL,
       source TEXT
     );
     CREATE INDEX changes_at ON changes(at);
     CREATE INDEX changes_artist_dir ON changes(artist_dir, at);
     CREATE INDEX changes_run ON changes(run_id);",
    ),
    // `gatefold run`: each run's log, and the daemon's own settings
    // (`paused`, `last_scheduled_at`).
    M::up(
        "CREATE TABLE run_lines (
           id INTEGER PRIMARY KEY,
           run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
           at INTEGER NOT NULL,
           kind TEXT NOT NULL,          -- section, ok, info, error
           text TEXT NOT NULL
         );
         CREATE INDEX run_lines_run ON run_lines(run_id, id);
         CREATE TABLE daemon (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    ),
    // `config.toml` as saved from the web UI or found changed on disk.
    M::up(
        "CREATE TABLE config_revisions (
           rev INTEGER PRIMARY KEY,
           created_at INTEGER NOT NULL,
           source TEXT NOT NULL,        -- file, web, rollback
           text TEXT NOT NULL
         );",
    ),
    // `gatefold run`'s jobs: which one a run was.
    M::up("ALTER TABLE runs ADD COLUMN job TEXT;"),
];

const MIGRATIONS: Migrations<'static> = Migrations::from_slice(MIGRATION_LIST);

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub struct Db {
    conn: Connection,
}

/// A change as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    pub id: i64,
    pub run_id: Option<i64>,
    pub at: i64,
    pub change: Change,
}

/// A run as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: i64,
    pub trigger: String,
    /// The `gatefold run` job it was, if any.
    pub job: Option<String>,
    pub scope_dir: Option<String>,
    pub dry_run: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub summary_json: Option<String>,
    pub error: Option<String>,
    pub changes: i64,
}

/// A line of a run's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRow {
    pub at: i64,
    pub kind: String,
    pub text: String,
}

/// A saved `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub rev: i64,
    pub created_at: i64,
    pub source: String,
    pub text: String,
}

/// How many runs keep their full log; older ones keep only their summary.
const KEEP_LOGS: i64 = 50;

/// Which changes to list.
#[derive(Debug, Default, Clone)]
pub struct ChangeQuery<'a> {
    /// Changes in this artist folder (or its album folders).
    pub artist_dir: Option<&'a Path>,
    /// The artist's name contains this (any case).
    pub artist: Option<&'a str>,
    pub run_id: Option<i64>,
    pub kinds: &'a [Kind],
    pub limit: usize,
    pub offset: usize,
}

impl Db {
    /// Open (creating) `gatefold.db` in `state_dir`, readable only by you.
    pub fn open_in(state_dir: &Path) -> Result<Self> {
        Self::open(&state_dir.join(FILE))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        MIGRATIONS
            .to_latest(&mut conn)
            .context("migrating gatefold.db")?;
        Ok(Self { conn })
    }

    /// Note a run starting; returns its id.
    pub fn start_run(
        &self,
        (trigger, job): (&str, Option<&str>),
        scope_dir: &Path,
        dry_run: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (trigger, job, scope_dir, dry_run, started_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
            params![trigger, job, scope_dir.to_string_lossy(), dry_run, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Note how a run ended (`ok`, `failed`, `stopped`).
    pub fn finish_run(
        &self,
        id: i64,
        status: &str,
        summary_json: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at = ?2, status = ?3, summary_json = ?4, error = ?5
             WHERE id = ?1",
            params![id, now(), status, summary_json, error],
        )?;
        Ok(())
    }

    pub fn insert_change(&self, run_id: Option<i64>, at: i64, change: &Change) -> Result<()> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
        self.conn.execute(
            "INSERT INTO changes (run_id, at, kind, action, artist, artist_dir, album_dir, file, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                run_id,
                at,
                change.kind.as_str(),
                change.action.as_str(),
                change.artist,
                path(&change.artist_dir),
                path(&change.album_dir),
                change.file.to_string_lossy(),
                change.source,
            ],
        )?;
        Ok(())
    }

    pub fn insert_line(&self, run_id: i64, kind: &str, text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO run_lines (run_id, at, kind, text) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, now(), kind, text],
        )?;
        Ok(())
    }

    /// Runs, newest first.
    pub fn runs(&self, limit: usize, offset: usize) -> Result<Vec<RunRow>> {
        self.query_runs(
            "ORDER BY r.id DESC LIMIT ?1 OFFSET ?2",
            params![limit_arg(limit), limit_arg(offset)],
        )
    }

    pub fn run(&self, id: i64) -> Result<Option<RunRow>> {
        Ok(self
            .query_runs("WHERE r.id = ?1", params![id])?
            .into_iter()
            .next())
    }

    fn query_runs(&self, tail: &str, args: impl rusqlite::Params) -> Result<Vec<RunRow>> {
        let sql = format!(
            "SELECT r.id, r.trigger, r.scope_dir, r.dry_run, r.started_at, r.finished_at,
                    r.status, r.summary_json, r.error,
                    (SELECT COUNT(*) FROM changes c WHERE c.run_id = r.id), r.job
             FROM runs r {tail}"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(args, |row| {
            Ok(RunRow {
                id: row.get(0)?,
                trigger: row.get(1)?,
                scope_dir: row.get(2)?,
                dry_run: row.get(3)?,
                started_at: row.get(4)?,
                finished_at: row.get(5)?,
                status: row.get(6)?,
                summary_json: row.get(7)?,
                error: row.get(8)?,
                changes: row.get(9)?,
                job: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A run's log, oldest first; `kind` filters (`ok`, `info`, `error`,
    /// `section`). Returns the page and how many lines match.
    pub fn lines(
        &self,
        run_id: i64,
        kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<LineRow>, i64)> {
        let filter = if kind.is_some() {
            "AND (kind = ?2 OR kind = 'section')"
        } else {
            "AND ?2 IS NULL"
        };
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM run_lines WHERE run_id = ?1 {filter} AND kind != 'section'"
            ),
            params![run_id, kind],
            |row| row.get(0),
        )?;
        let mut statement = self.conn.prepare(&format!(
            "SELECT at, kind, text FROM run_lines WHERE run_id = ?1 {filter}
             ORDER BY id LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = statement.query_map(
            params![run_id, kind, limit_arg(limit), limit_arg(offset)],
            |row| {
                Ok(LineRow {
                    at: row.get(0)?,
                    kind: row.get(1)?,
                    text: row.get(2)?,
                })
            },
        )?;
        Ok((rows.collect::<rusqlite::Result<_>>()?, total))
    }

    /// A job's latest run.
    pub fn last_run_of(&self, job: &str) -> Result<Option<RunRow>> {
        Ok(self
            .query_runs("WHERE r.job = ?1 ORDER BY r.id DESC LIMIT 1", params![job])?
            .into_iter()
            .next())
    }

    /// Runs left `running` by a gatefold that stopped without saying so
    /// (killed, crashed): they're `interrupted`. Returns how many.
    pub fn interrupt_running(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE runs SET status = 'interrupted', finished_at = COALESCE(finished_at, ?1)
             WHERE status = 'running'",
            [now()],
        )?)
    }

    /// Drop the logs of all but the newest [`KEEP_LOGS`] runs.
    pub fn prune_logs(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM run_lines WHERE run_id NOT IN
               (SELECT id FROM runs ORDER BY id DESC LIMIT ?1)",
            [KEEP_LOGS],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT value FROM daemon WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn add_revision(&self, source: &str, text: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO config_revisions (created_at, source, text) VALUES (?1, ?2, ?3)",
            params![now(), source, text],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Revisions, newest first.
    pub fn revisions(&self, limit: usize) -> Result<Vec<Revision>> {
        let mut statement = self.conn.prepare(
            "SELECT rev, created_at, source, text FROM config_revisions ORDER BY rev DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit_arg(limit)], |row| {
            Ok(Revision {
                rev: row.get(0)?,
                created_at: row.get(1)?,
                source: row.get(2)?,
                text: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revision(&self, rev: i64) -> Result<Option<Revision>> {
        Ok(self
            .revisions(usize::MAX)?
            .into_iter()
            .find(|r| r.rev == rev))
    }

    /// The revision `text` is: the newest, or a new one (`file`) when the
    /// file changed since.
    pub fn sync_revision(&self, text: &str) -> Result<i64> {
        match self.revisions(1)?.into_iter().next() {
            Some(latest) if latest.text == text => Ok(latest.rev),
            _ => self.add_revision("file", text),
        }
    }

    /// How many changes match (for paging).
    pub fn count_changes(&self, query: &ChangeQuery) -> Result<i64> {
        let (sql, args) = change_filter("SELECT COUNT(*) FROM changes WHERE 1 = 1", query);
        Ok(self.conn.query_row(
            &sql,
            rusqlite::params_from_iter(args.iter().map(AsRef::as_ref)),
            |row| row.get(0),
        )?)
    }

    /// Changes, newest first.
    pub fn changes(&self, query: &ChangeQuery) -> Result<Vec<ChangeRow>> {
        let (mut sql, mut args) = change_filter(
            "SELECT id, run_id, at, kind, action, artist, artist_dir, album_dir, file, source
             FROM changes WHERE 1 = 1",
            query,
        );
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT ? OFFSET ?");
        let limit = if query.limit == 0 { 100 } else { query.limit };
        args.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
        args.push(Box::new(i64::try_from(query.offset).unwrap_or(0)));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(args.iter().map(AsRef::as_ref)),
            |row| {
                let kind: String = row.get(3)?;
                let action: String = row.get(4)?;
                let dir = |i: usize| -> rusqlite::Result<Option<PathBuf>> {
                    Ok(row.get::<_, Option<String>>(i)?.map(PathBuf::from))
                };
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    kind,
                    action,
                    Change {
                        kind: Kind::Kept,
                        action: Action::Created,
                        artist: row.get(5)?,
                        artist_dir: dir(6)?,
                        album_dir: dir(7)?,
                        file: PathBuf::from(row.get::<_, String>(8)?),
                        source: row.get(9)?,
                    },
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (id, run_id, at, kind, action, mut change) = row?;
            // Rows from a newer gatefold with kinds this one doesn't know are left out.
            let (Some(kind), Some(action)) = (Kind::parse(&kind), Action::parse(&action)) else {
                continue;
            };
            change.kind = kind;
            change.action = action;
            out.push(ChangeRow {
                id,
                run_id,
                at,
                change,
            });
        }
        Ok(out)
    }

    /// A run's status.
    #[cfg(test)]
    pub fn run_status(&self, id: i64) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT status FROM runs WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?)
    }
}

fn limit_arg(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

type Args = Vec<Box<dyn rusqlite::ToSql>>;

/// `head` with the conditions of `query` appended.
fn change_filter(head: &str, query: &ChangeQuery) -> (String, Args) {
    let mut sql = String::from(head);
    let mut args: Args = Vec::new();
    if let Some(dir) = query.artist_dir {
        let dir = dir.to_string_lossy().into_owned();
        sql.push_str(" AND (artist_dir = ? OR album_dir = ? OR album_dir LIKE ? ESCAPE '\\')");
        args.push(Box::new(dir.clone()));
        args.push(Box::new(dir.clone()));
        args.push(Box::new(format!("{}/%", like_escape(&dir))));
    }
    if let Some(artist) = query.artist {
        sql.push_str(" AND artist LIKE ? ESCAPE '\\'");
        args.push(Box::new(format!("%{}%", like_escape(artist))));
    }
    if let Some(run) = query.run_id {
        sql.push_str(" AND run_id = ?");
        args.push(Box::new(run));
    }
    if !query.kinds.is_empty() {
        let marks = vec!["?"; query.kinds.len()].join(", ");
        let _ = write!(sql, " AND kind IN ({marks})");
        for kind in query.kinds {
            args.push(Box::new(kind.as_str()));
        }
    }
    (sql, args)
}

/// Escape `%`, `_` and `\` for a `LIKE … ESCAPE '\'` pattern.
fn like_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(kind: Kind, artist: &str, artist_dir: &str, album: Option<&str>) -> Change {
        Change {
            kind,
            action: Action::Created,
            artist: Some(artist.to_string()),
            artist_dir: Some(PathBuf::from(artist_dir)),
            album_dir: album.map(PathBuf::from),
            file: PathBuf::from(format!("{artist_dir}/x")),
            source: Some("apple music".to_string()),
        }
    }

    #[test]
    fn migrations_are_valid() {
        assert!(MIGRATIONS.validate().is_ok());
    }

    #[test]
    fn records_runs_and_lists_changes_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let run = db
            .start_run(("cli", None), Path::new("/music"), false)
            .unwrap();
        assert_eq!(db.run_status(run).unwrap().as_deref(), Some("running"));

        let afi = change(Kind::ArtistLogo, "AFI", "/music/AFI", None);
        let cover = change(
            Kind::CoverVideo,
            "The Interrupters",
            "/music/The Interrupters",
            Some("/music/The Interrupters/Battle Scars"),
        );
        db.insert_change(Some(run), 10, &afi).unwrap();
        db.insert_change(Some(run), 20, &cover).unwrap();
        db.finish_run(run, "ok", Some("{}"), None).unwrap();
        assert_eq!(db.run_status(run).unwrap().as_deref(), Some("ok"));

        let all = db.changes(&ChangeQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|r| r.change.kind).collect::<Vec<_>>(),
            [Kind::CoverVideo, Kind::ArtistLogo]
        );
        assert_eq!(all[0].change, cover);
        assert_eq!(all[0].run_id, Some(run));

        let by_dir = ChangeQuery {
            artist_dir: Some(Path::new("/music/AFI")),
            ..ChangeQuery::default()
        };
        assert_eq!(db.changes(&by_dir).unwrap().len(), 1);
        let by_name = ChangeQuery {
            artist: Some("interrupt"),
            ..ChangeQuery::default()
        };
        assert_eq!(db.changes(&by_name).unwrap()[0].change, cover);
        let by_kind = ChangeQuery {
            kinds: &[Kind::ArtistLogo],
            ..ChangeQuery::default()
        };
        assert_eq!(db.changes(&by_kind).unwrap()[0].change, afi);
    }

    #[test]
    fn an_album_folder_counts_as_its_artists() {
        let db = Db::open_in_memory().unwrap();
        // A change with no artist folder (none above the album) still shows up
        // under the folder the album sits in.
        let mut lonely = change(
            Kind::Lrc,
            "Paramore",
            "/x",
            Some("/music/Paramore/Paramore (2013)"),
        );
        lonely.artist_dir = None;
        db.insert_change(None, 1, &lonely).unwrap();
        let query = ChangeQuery {
            artist_dir: Some(Path::new("/music/Paramore")),
            ..ChangeQuery::default()
        };
        assert_eq!(db.changes(&query).unwrap().len(), 1);
        // `_` in a name isn't a wildcard.
        let query = ChangeQuery {
            artist_dir: Some(Path::new("/music/Param_re")),
            ..ChangeQuery::default()
        };
        assert_eq!(db.changes(&query).unwrap(), []);
    }
}
