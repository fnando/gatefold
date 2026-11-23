//! `gatefold run`: runs the config's jobs over the library, each on its own
//! schedule and in parallel (a job never overlaps itself), runs what the web
//! UI asks for, and serves the web UI (see [`crate::web`]). Every run is
//! recorded in `gatefold.db` and reported live, in its job's *lane*.
//!
//! Parallel runs stay out of each other's way: requests to a service are
//! paced across all of them (`net`), and a folder is worked on by one run at
//! a time (`folders`).

use crate::cli::Cli;
use crate::config::Job;
use crate::db::{self, Db};
use crate::{config, events, lock, shutdown::Shutdown};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// The lane runs asked from the web UI (not a job) go in.
pub const MANUAL: &str = "manual";

/// What a run asked from the UI does differently from the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // the run's flags, as on the command line
pub struct Options {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub replace_lyrics: bool,
    #[serde(default)]
    pub replace_cover: bool,
    #[serde(default)]
    pub replace_artist_images: bool,
    /// `lyrics`, `cover` or `artist`: only that part.
    #[serde(default)]
    pub only: Option<String>,
}

/// A run to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `schedule` or `manual`.
    pub trigger: &'static str,
    /// The job it is (its options and lane); `None`: a run in the manual lane.
    pub job: Option<String>,
    /// A folder to cover instead of the whole library.
    pub dir: Option<PathBuf>,
    pub options: Options,
}

/// A job, as the daemon runs it.
#[derive(Debug, Clone)]
pub struct JobLane {
    pub name: String,
    pub job: Job,
    pub interval: Option<Duration>,
}

/// The daemon's state, shared with the web UI.
pub struct Daemon {
    /// The command line the daemon was started with (config applied); each
    /// run starts from it, with its job's options on top.
    pub base: Cli,
    pub state_dir: PathBuf,
    /// Library folders (canonical): what a run covers, and the only places
    /// the web UI reads or changes files in.
    pub roots: Vec<PathBuf>,
    pub jobs: Vec<JobLane>,
    /// Runs asked for in the manual lane, oldest first.
    manual: Mutex<VecDeque<Request>>,
    /// Jobs asked to run now.
    asked: Mutex<BTreeSet<String>>,
    /// Lanes with a run going on.
    busy: Mutex<BTreeSet<String>>,
    wake: Notify,
    /// File names in the wrong case (see [`crate::casefix`]), as of the last
    /// check (at start, and after each run).
    problems: Mutex<Vec<crate::casefix::Problem>>,
}

const PAUSED: &str = "paused";

fn last_scheduled_key(job: &str) -> String {
    format!("last_scheduled_at:{job}")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Daemon {
    pub fn new(base: Cli, state_dir: PathBuf, roots: Vec<PathBuf>, jobs: Vec<JobLane>) -> Self {
        Self {
            base,
            state_dir,
            roots,
            jobs,
            manual: Mutex::default(),
            asked: Mutex::default(),
            busy: Mutex::default(),
            wake: Notify::new(),
            problems: Mutex::default(),
        }
    }

    pub fn db(&self) -> Result<Db> {
        Db::open_in(&self.state_dir)
    }

    pub fn job(&self, name: &str) -> Option<&JobLane> {
        self.jobs.iter().find(|job| job.name == name)
    }

    pub fn running(&self, lane: &str) -> bool {
        lock(&self.busy).contains(lane)
    }

    /// Ask for a run in the manual lane; returns what to tell the user.
    pub fn request(&self, request: Request) -> String {
        let mut queue = lock(&self.manual);
        if queue.contains(&request) {
            return "That run is already waiting to start.".to_string();
        }
        queue.push_back(request);
        let waiting = queue.len();
        drop(queue);
        self.wake.notify_one();
        if self.running(MANUAL) {
            format!("Queued: it starts when the current one is done ({waiting} waiting).")
        } else {
            "Starting a run.".to_string()
        }
    }

    /// Ask for job `name` to run now (once its current run, if any, is done).
    pub fn run_job(&self, name: &str) -> Option<String> {
        self.job(name)?;
        lock(&self.asked).insert(name.to_string());
        self.wake.notify_one();
        Some(if self.running(name) {
            format!("{name} runs again when its current run is done.")
        } else {
            format!("Starting {name}.")
        })
    }

    /// Look for file names in the wrong case across the library. Blocking.
    pub fn check_case(&self) {
        let found: Vec<_> = self
            .roots
            .iter()
            .flat_map(|root| crate::casefix::scan(root))
            .collect();
        *lock(&self.problems) = found;
    }

    pub fn problems(&self) -> Vec<crate::casefix::Problem> {
        lock(&self.problems).clone()
    }

    pub fn queued(&self) -> usize {
        lock(&self.manual).len()
    }

    /// Stop the run in `lane` (every run, with `None`) after its current track
    /// or artist. Stopping the manual lane drops its waiting runs too.
    pub fn stop(&self, lane: Option<&str>) -> String {
        if lane.is_none_or(|lane| lane == MANUAL) {
            lock(&self.manual).clear();
        }
        match lane {
            Some(lane) => {
                lock(&self.asked).remove(lane);
            }
            None => lock(&self.asked).clear(),
        }
        match events::request_stop(lane) {
            0 => "Nothing is running.".to_string(),
            1 => "Stopping after the current track or artist.".to_string(),
            n => format!("Stopping {n} runs after their current track or artist."),
        }
    }

    pub fn paused(&self) -> bool {
        self.db()
            .and_then(|db| db.setting(PAUSED))
            .is_ok_and(|v| v.as_deref() == Some("1"))
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.db()?
            .set_setting(PAUSED, if paused { "1" } else { "0" })?;
        self.wake.notify_one();
        Ok(())
    }

    /// When job `name` is next due (unix seconds), if it has a schedule and
    /// scheduled runs aren't paused. Never run before: now.
    pub fn next_scheduled(&self, name: &str) -> Option<i64> {
        let interval = i64::try_from(self.job(name)?.interval?.as_secs()).ok()?;
        if self.paused() {
            return None;
        }
        let last = self
            .db()
            .ok()?
            .setting(&last_scheduled_key(name))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok());
        Some(last.map_or_else(db::now, |last| last + interval))
    }

    /// The runs to start now: each idle job that's asked for or due, and the
    /// next manual run when that lane is idle. Marks their lanes busy.
    fn due(&self) -> Vec<(String, Request)> {
        let mut busy = lock(&self.busy);
        let mut start = Vec::new();
        for job in &self.jobs {
            if busy.contains(&job.name) {
                continue;
            }
            let asked = lock(&self.asked).remove(&job.name);
            let scheduled = !asked
                && self
                    .next_scheduled(&job.name)
                    .is_some_and(|due| due <= db::now());
            if !asked && !scheduled {
                continue;
            }
            if scheduled {
                let _ = self.db().and_then(|db| {
                    db.set_setting(&last_scheduled_key(&job.name), &db::now().to_string())
                });
            }
            busy.insert(job.name.clone());
            start.push((
                job.name.clone(),
                Request {
                    trigger: if asked { "manual" } else { "schedule" },
                    job: Some(job.name.clone()),
                    dir: None,
                    options: Options::default(),
                },
            ));
        }
        if !busy.contains(MANUAL)
            && let Some(request) = lock(&self.manual).pop_front()
        {
            busy.insert(MANUAL.to_string());
            start.push((MANUAL.to_string(), request));
        }
        start
    }

    fn done(&self, lane: &str) {
        lock(&self.busy).remove(lane);
        self.wake.notify_one();
    }

    /// The command line for one run over `dir`.
    fn cli_for(&self, dir: &Path, request: &Request) -> Cli {
        let mut cli = Cli {
            command: None,
            dir: dir.to_path_buf(),
            ..self.base.clone()
        };
        if let Some(job) = request.job.as_deref().and_then(|name| self.job(name)) {
            job.job.apply(&mut cli);
        }
        let options = &request.options;
        cli.dry_run |= options.dry_run;
        cli.replace_lyrics |= options.replace_lyrics;
        cli.replace_cover |= options.replace_cover;
        cli.replace_artist_images |= options.replace_artist_images;
        match options.only.as_deref() {
            Some("lyrics") => cli.lyrics_only = true,
            Some("cover") => cli.cover_only = true,
            Some("artist") => cli.artist_images_only = true,
            _ => {}
        }
        cli
    }

    /// Do `request` in `lane`: a run per library folder (or the one folder
    /// asked for). Blocking.
    fn execute(&self, lane: &str, request: &Request) {
        let _lane = events::enter(lane);
        let dirs = request
            .dir
            .clone()
            .map_or_else(|| self.roots.clone(), |dir| vec![dir]);
        for dir in dirs {
            if events::stopping() {
                break;
            }
            let cli = self.cli_for(&dir, request);
            let recorder = crate::open_recorder_for(
                &self.state_dir,
                &cli,
                (request.trigger, request.job.as_deref()),
            );
            let id = recorder.as_ref().and_then(|r| r.run_id());
            events::emit(events::Event::RunStarted {
                id,
                trigger: request.trigger.to_string(),
                dir: dir.display().to_string(),
            });
            let result = crate::run(&cli, &self.state_dir, recorder.clone());
            let status = crate::run_status(&result);
            if let Some(recorder) = &recorder {
                crate::finish_recorded_run(recorder, &result);
            }
            if let Err(err) = &result {
                eprintln!("✗ {lane}: run over {}: {err:#}", dir.display());
            }
            events::emit(events::Event::RunFinished {
                id,
                status: status.to_string(),
            });
        }
        if let Err(err) = self.db().and_then(|db| db.prune_logs()) {
            eprintln!("warning: couldn't prune old run logs: {err:#}");
        }
        self.check_case();
    }
}

/// `gatefold run`: lock the state dir, then run the jobs and serve the web UI
/// until SIGINT/SIGTERM.
pub fn start(cli: Cli) -> Result<()> {
    let config = config::get();
    let state_dir = crate::resolve_state_dir(cli.state_dir.as_ref())?;
    let _lock = lock::acquire(&state_dir.join("gatefold.lock"))?;
    let interrupted = Db::open_in(&state_dir)?.interrupt_running()?;
    if interrupted > 0 {
        eprintln!("· {interrupted} run(s) were cut short last time; marked interrupted");
    }
    let libraries = match config.library() {
        dirs if dirs.is_empty() => vec![cli.dir.clone()],
        dirs => dirs,
    };
    let roots = libraries
        .iter()
        .map(|dir| {
            std::fs::canonicalize(dir).with_context(|| format!("library folder {}", dir.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let jobs = config
        .jobs()
        .into_iter()
        .map(|(name, job, interval)| JobLane {
            name,
            job,
            interval,
        })
        .collect();
    events::enable();
    let daemon = Arc::new(Daemon::new(cli, state_dir, roots, jobs));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(daemon))
}

async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let shutdown = Shutdown::listen();
    // The first signal stops every run after its current track or artist;
    // the second exits right away.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            shutdown.stopping().await;
            events::request_stop(None);
            shutdown.killing().await;
            std::process::exit(shutdown.code().unwrap_or(130));
        });
    }

    let config = config::get();
    let server = if config.server.enabled {
        let listener = tokio::net::TcpListener::bind(&config.server.listen)
            .await
            .with_context(|| format!("listening on {}", config.server.listen))?;
        let addr = listener.local_addr()?;
        eprintln!("web UI at http://{addr}");
        let web = Arc::new(crate::web::Web::new(
            daemon.clone(),
            config.token().unwrap_or_default(),
            addr,
            shutdown.clone(),
        ));
        Some(tokio::spawn(crate::web::serve(
            web,
            listener,
            shutdown.clone(),
        )))
    } else {
        None
    };
    eprintln!("over {}", library_list(&daemon.roots));
    if daemon.jobs.is_empty() {
        eprintln!("no jobs: runs only when asked");
    }
    for job in &daemon.jobs {
        match job.interval {
            Some(every) => eprintln!(
                "job {}: every {}",
                job.name,
                crate::cli::format_duration(every)
            ),
            None => eprintln!("job {}: only when asked", job.name),
        }
    }

    {
        let daemon = daemon.clone();
        tokio::task::spawn_blocking(move || daemon.check_case());
    }
    let mut running = tokio::task::JoinSet::new();
    while !shutdown.is_stopping() {
        for (lane, request) in daemon.due() {
            let worker = daemon.clone();
            running.spawn_blocking(move || {
                worker.execute(&lane, &request);
                worker.done(&lane);
            });
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(30)) => {}
            () = daemon.wake.notified() => {}
            () = shutdown.stopping() => {}
            Some(_) = running.join_next(), if !running.is_empty() => {}
        }
    }
    // Every run was asked to stop; let them finish their track or artist.
    while running.join_next().await.is_some() {}
    if let Some(server) = server {
        let _ = server.await;
    }
    Ok(())
}

fn library_list(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn daemon(name: &str, jobs: Vec<JobLane>) -> (Daemon, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("gatefold-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cli = Cli::parse_from(["gatefold"]);
        (Daemon::new(cli, dir.clone(), vec![dir.clone()], jobs), dir)
    }

    fn job(name: &str, every: Option<u64>, job: Job) -> JobLane {
        JobLane {
            name: name.to_string(),
            job,
            interval: every.map(Duration::from_secs),
        }
    }

    fn manual() -> Request {
        Request {
            trigger: "manual",
            job: None,
            dir: None,
            options: Options::default(),
        }
    }

    #[test]
    fn manual_runs_queue_one_at_a_time() {
        let (daemon, dir) = daemon("queue", Vec::new());
        daemon.request(manual());
        assert_eq!(
            daemon.request(manual()),
            "That run is already waiting to start."
        );
        let started = daemon.due();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, MANUAL);
        assert!(daemon.running(MANUAL));
        let mut other = manual();
        other.options.dry_run = true;
        daemon.request(other);
        assert_eq!(daemon.due().len(), 0, "the manual lane is busy");
        daemon.done(MANUAL);
        assert_eq!(daemon.due().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jobs_run_in_parallel_but_never_overlap_themselves() {
        let jobs = vec![
            job("new-tracks", Some(300), Job::default()),
            job("artist-images", Some(3600), Job::default()),
            job("by-hand", None, Job::default()),
        ];
        let (daemon, dir) = daemon("jobs", jobs);
        // Never ran: both scheduled jobs are due now; the unscheduled one isn't.
        let started: Vec<String> = daemon.due().into_iter().map(|(lane, _)| lane).collect();
        assert_eq!(started, ["new-tracks", "artist-images"]);
        assert_eq!(daemon.due().len(), 0, "running, and just scheduled");

        // Asked again while running: it waits for its run to end.
        assert!(
            daemon
                .run_job("new-tracks")
                .unwrap()
                .contains("when its current run is done")
        );
        assert_eq!(daemon.due().len(), 0);
        daemon.done("new-tracks");
        let again = daemon.due();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].1.trigger, "manual");
        assert_eq!(again[0].1.job.as_deref(), Some("new-tracks"));

        assert!(daemon.run_job("by-hand").is_some());
        assert_eq!(daemon.due()[0].0, "by-hand");
        assert!(daemon.run_job("nope").is_none());

        let next = daemon.next_scheduled("artist-images").unwrap();
        assert!((next - db::now() - 3600).abs() <= 1);
        daemon.set_paused(true).unwrap();
        assert_eq!(daemon.next_scheduled("artist-images"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_jobs_options_shape_its_runs() {
        let artist_images = Job {
            only: Some(config::Only::ArtistImages),
            replace: Some(config::Replace::Only(vec![
                config::Replaceable::ArtistImages,
            ])),
            ..Job::default()
        };
        let (daemon, dir) = daemon("options", vec![job("artist-images", None, artist_images)]);
        let request = Request {
            trigger: "schedule",
            job: Some("artist-images".into()),
            dir: None,
            options: Options::default(),
        };
        let cli = daemon.cli_for(Path::new("/music"), &request);
        assert!(cli.artist_images_only && cli.replace_artist_images);
        assert!(!cli.replace_cover && !cli.dry_run);

        let options = Options {
            dry_run: true,
            replace_cover: true,
            only: Some("cover".into()),
            ..Options::default()
        };
        let cli = daemon.cli_for(
            Path::new("/music/AFI"),
            &Request {
                trigger: "manual",
                job: None,
                dir: Some("/music/AFI".into()),
                options,
            },
        );
        assert_eq!(cli.dir, PathBuf::from("/music/AFI"));
        assert!(cli.dry_run && cli.replace_cover && cli.cover_only);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
