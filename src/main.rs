mod apple;
mod archive;
mod artist;
mod backup;
mod cli;
mod coverart;
mod jellyfin;
mod keep;
mod lossless;
mod lrclib;
mod metadata;
mod musicbrainz;
mod net;
mod nfo;
mod optimize;
mod paths;
mod progress;
mod restore;
mod revision;
mod sample;
mod scanner;
mod skip;
mod state;
mod verify;
mod video;
mod wikidata;
mod writer;

use anyhow::{Context, Result};
use artist::{ArtistRecord, Resolved, Source};
use backup::Backup;
use clap::Parser;
use cli::Cli;
use lrclib::{Client, Fetched, LyricsRecord, Query, Unauthorized};
use metadata::{Fingerprint, TrackMetadata};
use progress::Progress;
use revision::Revisions;
use scanner::Track;
use state::{State, digest_for, digest_str};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use writer::ArtistImage;

fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();

    if let Some(command) = &cli.command {
        return run_command(command);
    }
    let state_dir = resolve_state_dir(cli.state_dir.as_ref())?;
    let state_dir = state_dir.as_path();

    if cli.verify {
        verify::ensure_available()?;
    }

    let state = State::new(state_dir)
        .with_context(|| format!("preparing state dir {}", state_dir.display()))?;

    let delay = Duration::from_millis(cli.delay_ms);
    // Seed per-track estimate: one request per enabled step plus rough RTT. With
    // only artist images enabled the track loop just reads (cached) metadata.
    let track_requests = f64::from(u8::from(cli.lyrics_enabled()) + u8::from(cli.cover_enabled()));
    let seed_secs = if track_requests > 0.0 {
        delay.as_secs_f64().mul_add(track_requests, 1.0)
    } else {
        0.05
    };

    // Canonical so artist-folder matching can walk up to (and include) the root
    // even when it was given as `.`.
    let root = std::fs::canonicalize(&cli.dir).unwrap_or_else(|_| cli.dir.clone());
    let tracks = scanner::scan(&root);
    // Artist images alone only need each album folder's album artist: read a
    // few tracks per folder rather than every one.
    let artist_only = !cli.lyrics_enabled() && !cli.cover_enabled();
    let folders = artist_only.then(|| sample::by_folder(&tracks));
    let to_read = folders
        .as_ref()
        .map_or(tracks.len(), |folders| folders.iter().map(Vec::len).sum());
    let bar = if artist_only {
        "reading tags"
    } else {
        "processing"
    };
    let progress = Progress::new(bar, to_read, seed_secs);

    let mb_client = musicbrainz::Client::new(&cli.musicbrainz_url, cli.user_agent(), delay);
    let mb_host = (!mb_client.is_public()).then(|| mb_client.host());
    progress.headline(&run_summary(&cli, mb_host.as_deref()));

    log_scan(&progress, &root, &tracks, folders.as_deref(), to_read);

    // Reading a few tags per folder is quick, and "Scanning" already says what
    // is read, so an artist-only run has no estimate or section of its own here.
    if !artist_only {
        progress.section("Estimating");
        progress.log_initial_estimate();
    }

    let services = Services::new(&cli, mb_client, delay, &progress);
    let revisions = Revisions::new();

    if !artist_only {
        progress.section(&section_title(&cli, "Processing"));
    }

    let mut summary = Summary::default();
    let artist_jobs = if let Some(folders) = &folders {
        run_artist_scan(&root, &state, &progress, folders, &mut summary)?
    } else {
        run_track_pass(
            &cli,
            &root,
            &state,
            (&services, &revisions),
            &progress,
            &tracks,
            &mut summary,
        )?
    };
    progress.finish();

    if let Some(apple_id) = &cli.apple_url {
        pin_apple_id(&state, &progress, &artist_jobs, apple_id)?;
    }

    if cli.artist_images_enabled() {
        run_artist_pass(
            &cli,
            &state,
            &services,
            &revisions,
            &progress,
            delay,
            &artist_jobs,
            &mut summary,
        );
    }

    if cli.write_nfo && cli.artist_images_enabled() {
        summary.nfo = Some(run_nfo_pass(
            &state,
            &progress,
            (&root, &tracks),
            &artist_jobs,
            cli.dry_run,
        ));
    }

    if !cli.skip_jellyfin
        && cli.artist_images_enabled()
        && let Some(server) = cli.jellyfin.server()
    {
        summary.jellyfin = Some(run_jellyfin_pass(
            &cli,
            server,
            &state,
            &revisions,
            &progress,
            &artist_jobs,
        ));
    }

    progress.section("Summary");
    summary.print(&progress, &cli);
    Ok(())
}

/// Counts for one kind of nfo file.
#[derive(Debug, Default)]
struct NfoTally {
    wrote: u64,
    completed: u64,
    already: u64,
    skipped: u64,
    errors: u64,
}

impl NfoTally {
    /// Count one file and log what needs saying; `file` names it
    /// (`artist.nfo`), `id` is the `MusicBrainz` id the tags give.
    fn record(
        &mut self,
        progress: &Progress,
        (label, file, id): (&str, &str, &str),
        result: Result<nfo::Outcome>,
        dry_run: bool,
    ) {
        match result {
            Ok(nfo::Outcome::Wrote) => {
                self.wrote += 1;
                let wrote = act(dry_run, "wrote", "write");
                progress.info(format!("✓ {label}: {wrote} {file}"));
            }
            Ok(nfo::Outcome::Completed) => {
                self.completed += 1;
                let completed = act(dry_run, "completed", "complete");
                progress.info(format!("✓ {label}: {completed} {file}"));
            }
            Ok(nfo::Outcome::Already) => self.already += 1,
            Ok(nfo::Outcome::Differs(found)) => {
                self.already += 1;
                progress.info(format!(
                    "· {label}: {file} names another MusicBrainz id ({found}, tags say {id})"
                ));
            }
            Ok(nfo::Outcome::WrongRoot) => {
                self.skipped += 1;
                progress.info(format!(
                    "· {label}: {file} isn't one gatefold reads, left alone"
                ));
            }
            Err(err) => {
                self.errors += 1;
                progress.info(format!("✗ {label}: {file}: {err:#}"));
            }
        }
    }

    fn summary(&self, file: &str, dry_run: bool) -> String {
        format!(
            "{} {} {file}, {} {}, already {}, skipped {}, errors {}",
            act(dry_run, "wrote", "write"),
            self.wrote,
            act(dry_run, "completed", "complete"),
            self.completed,
            self.already,
            self.skipped,
            self.errors
        )
    }
}

/// What the nfo pass did, per kind of file.
#[derive(Debug, Default)]
struct NfoSummary {
    artists: NfoTally,
    albums: NfoTally,
}

/// `--write-nfo`: an `artist.nfo` in each artist folder found and an
/// `album.nfo` in each album folder (see [`nfo`]). Problems are logged; they
/// never fail the run.
fn run_nfo_pass(
    state: &State,
    progress: &Progress,
    (root, tracks): (&Path, &[Track]),
    jobs: &ArtistJobs,
    dry_run: bool,
) -> NfoSummary {
    progress.section(if dry_run { "nfo (dry run)" } else { "nfo" });
    let summary = NfoSummary {
        artists: write_artist_nfos(state, progress, jobs, dry_run),
        albums: write_album_nfos(state, progress, (root, tracks), jobs, dry_run),
    };
    let (artists, albums) = (&summary.artists, &summary.albums);
    if artists.wrote
        + artists.completed
        + artists.errors
        + albums.wrote
        + albums.completed
        + albums.errors
        == 0
    {
        progress.info(format!(
            "· nothing to write ({} already complete, {} skipped)",
            artists.already + albums.already,
            artists.skipped + albums.skipped
        ));
    }
    summary
}

/// An `artist.nfo` per artist folder, naming the artist and its `MusicBrainz` id.
fn write_artist_nfos(
    state: &State,
    progress: &Progress,
    jobs: &ArtistJobs,
    dry_run: bool,
) -> NfoTally {
    let mut tally = NfoTally::default();
    let mut claims: BTreeMap<&Path, usize> = BTreeMap::new();
    for job in jobs.by_id.values() {
        for folder in &job.folders {
            *claims.entry(folder.as_path()).or_default() += 1;
        }
    }
    progress.info(format!("{} artist folder(s)", claims.len()));
    for (folder, credit) in &jobs.joint {
        tally.skipped += 1;
        progress.info(joint_line(folder, credit, "no artist.nfo"));
    }
    for job in jobs.by_id.values() {
        let mb_name = state
            .load_musicbrainz_artist(&job.mbid)
            .ok()
            .flatten()
            .and_then(|record| record["name"].as_str().map(String::from));
        for folder in &job.folders {
            let label = folder_label(folder);
            let skip = nfo::artist_title(
                &label,
                mb_name.as_deref(),
                &job.name,
                claims[folder.as_path()],
            );
            let name = match skip {
                Ok(name) => name,
                Err(skip) => {
                    tally.skipped += 1;
                    match skip {
                        nfo::Skip::Compilation => {}
                        nfo::Skip::Shared => progress.info(format!(
                            "· {label}: no artist.nfo, albums of several artists live here"
                        )),
                        nfo::Skip::OtherName(name) => progress.info(format!(
                            "· {label}: no artist.nfo, {} is named \"{name}\"",
                            job.mbid
                        )),
                    }
                    continue;
                }
            };
            let result = nfo::write_artist(folder, name, &job.mbid, dry_run);
            let what = (label.as_str(), nfo::ARTIST_FILE, job.mbid.as_str());
            tally.record(progress, what, result, dry_run);
        }
    }
    tally
}

/// The log line for an artist folder named after a joint credit, skipped.
fn joint_line(folder: &Path, credit: &str, what: &str) -> String {
    format!(
        "· {}: {what}, joint credit \"{credit}\" (several artists), skipped",
        folder_label(folder)
    )
}

/// An `album.nfo` per album folder, from every track's tags (read from the
/// metadata cache, so only changed files are parsed again).
fn write_album_nfos(
    state: &State,
    progress: &Progress,
    (root, tracks): (&Path, &[Track]),
    jobs: &ArtistJobs,
    dry_run: bool,
) -> NfoTally {
    let mut tally = NfoTally::default();
    let mut read = Vec::with_capacity(tracks.len());
    for track in tracks {
        progress.status(track_name(track));
        match track_metadata(state, track) {
            Ok(meta) => read.push((track.path.clone(), meta)),
            Err(err) => {
                tally.errors += 1;
                progress.info(format!("✗ {}: {err:#}", track.path.display()));
            }
        }
    }
    let artist_folders: BTreeSet<PathBuf> = jobs
        .by_id
        .values()
        .flat_map(|job| job.folders.iter().cloned())
        .chain(jobs.joint.keys().cloned())
        .collect();
    let (albums, skipped) = nfo::album_folders(root, &artist_folders, &read);
    progress.info(format!("{} album folder(s)", albums.len()));
    for (folder, why) in skipped {
        tally.skipped += 1;
        let label = album_label(root, &folder);
        match why {
            nfo::AlbumSkip::Mixed => progress.info(format!(
                "· {label}: no album.nfo, tracks of several albums live here"
            )),
            // Loose tracks in an artist folder or the root: nothing to say.
            nfo::AlbumSkip::NotAnAlbumFolder => {}
        }
    }
    for (folder, metas) in albums {
        let album = nfo::album_from(&metas);
        let label = album_label(root, &folder);
        if let Some((credit, ids)) = nfo::joint_album_artist_tag(&metas) {
            progress.info(format!(
                "· {label}: ALBUMARTIST is one value, \"{credit}\", for {ids} artists; \
                 Jellyfin may rename the first one, so split it into one value per artist"
            ));
        }
        let id = album.release_id.clone().unwrap_or_default();
        let result = nfo::write_album(&folder, &album, dry_run);
        tally.record(progress, (&label, nfo::ALBUM_FILE, &id), result, dry_run);
    }
    tally
}

/// `Artist/Album` for an album folder under `root`.
fn album_label(root: &Path, folder: &Path) -> String {
    folder
        .strip_prefix(root)
        .ok()
        .filter(|rel| !rel.as_os_str().is_empty())
        .or_else(|| folder.file_name().map(Path::new))
        .unwrap_or(folder)
        .display()
        .to_string()
}

/// A track's tags, from the metadata cache when the file hasn't changed.
fn track_metadata(state: &State, track: &Track) -> Result<TrackMetadata> {
    let abs = std::fs::canonicalize(&track.path).unwrap_or_else(|_| track.path.clone());
    let digest = digest_for(&abs);
    let cached = state.load_metadata::<TrackMetadata>(&digest)?;
    refresh_metadata(state, track, &digest, cached)
}

/// `--state-dir`, else `$XDG_DATA_HOME/gatefold` or `~/.local/share/gatefold`.
fn resolve_state_dir(given: Option<&PathBuf>) -> Result<PathBuf> {
    match given {
        Some(dir) => Ok(dir.clone()),
        None => state::default_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
            .context("no --state-dir given and neither $XDG_DATA_HOME nor $HOME is set"),
    }
}

/// Make Jellyfin show the artist folders' images, after the artist pass wrote
/// them. Jellyfin trouble is logged; it never fails the run.
fn run_jellyfin_pass(
    cli: &Cli,
    (url, api_key): (&str, &str),
    state: &State,
    revisions: &Revisions,
    progress: &Progress,
    jobs: &ArtistJobs,
) -> jellyfin::Tally {
    progress.section("Jellyfin");
    let mut tally = jellyfin::Tally::default();
    let session = match jellyfin::Session::connect(url, api_key, cli.user_agent()) {
        Ok(session) => session,
        Err(err) => {
            tally.errors += 1;
            progress.info(format!("✗ Jellyfin at {url}: {err:#}"));
            return tally;
        }
    };
    let backups = state.backup_root().join("jellyfin");
    let opts = jellyfin_options(&backups, revisions, cli.dry_run);
    for job in jobs.by_id.values() {
        for folder in &job.folders {
            let result = session.sync_folder(folder, Some(&job.mbid), &opts);
            for line in tally.record(&job.name, result, cli.dry_run) {
                progress.info(line);
            }
        }
    }
    tally
}

fn jellyfin_options<'a>(
    backups: &'a Path,
    revisions: &Revisions,
    dry_run: bool,
) -> jellyfin::Options<'a> {
    jellyfin::Options {
        dry_run,
        backups,
        stamp: revisions.stamp(),
        saves_locally: false,
        poll: Duration::from_secs(1),
        polls: 15,
    }
}

/// After a restore: have Jellyfin rescan the artist, so it shows the restored
/// file even under the same name, then push its images (`gatefold jellyfin
/// refresh` on that folder). Trouble is reported; the restore itself already
/// succeeded.
fn rescan_in_jellyfin(dir: &Path, server: (&str, &str), state_dir: Option<&PathBuf>) {
    let print = |line: String| println!("  {line}");
    let dirs = [dir.to_path_buf()];
    if let Err(err) = sync_jellyfin(
        JellyfinAction::Refresh,
        &dirs,
        false,
        server,
        state_dir,
        print,
    ) {
        print(format!("✗ {}: Jellyfin: {err:#}", folder_label(dir)));
    }
}

/// `gatefold jellyfin push` / `pull` / `refresh`.
fn jellyfin_command(command: &cli::JellyfinCommand) -> Result<()> {
    let (action, target) = match command {
        cli::JellyfinCommand::Push(target) => (JellyfinAction::Push, target),
        cli::JellyfinCommand::Pull(target) => (JellyfinAction::Pull, target),
        cli::JellyfinCommand::Refresh(target) => (JellyfinAction::Refresh, target),
    };
    let server = target.jellyfin.server().context(
        "set the Jellyfin server with --jellyfin-url and --jellyfin-api-key (or GATEFOLD_JELLYFIN_URL / GATEFOLD_JELLYFIN_API_KEY)",
    )?;
    let (summary, errors) = sync_jellyfin(
        action,
        &target.dirs,
        target.dry_run,
        server,
        target.state_dir.as_ref(),
        |line| println!("{line}"),
    )?;
    println!("{summary}");
    if errors > 0 {
        anyhow::bail!("{errors} artist folder(s) could not be synced");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JellyfinAction {
    Push,
    Pull,
    /// Rescan in Jellyfin, then push.
    Refresh,
}

fn folder_label(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Run `action` on each artist folder under `dirs` (a folder that isn't an
/// artist in Jellyfin is read as a library of artist folders), handing each
/// result line to `out`. Returns the summary line and the error count.
fn sync_jellyfin(
    action: JellyfinAction,
    dirs: &[PathBuf],
    dry_run: bool,
    (url, api_key): (&str, &str),
    state_dir: Option<&PathBuf>,
    mut out: impl FnMut(String),
) -> Result<(String, u64)> {
    let state_dir = resolve_state_dir(state_dir)?;
    let state = State::new(&state_dir)
        .with_context(|| format!("preparing state dir {}", state_dir.display()))?;
    let backups = state.backup_root().join("jellyfin");
    let revisions = Revisions::new();
    let opts = jellyfin_options(&backups, &revisions, dry_run);
    let mut session = jellyfin::Session::connect(url, api_key, cli::default_user_agent())
        .with_context(|| format!("connecting to Jellyfin at {url}"))?;

    let mut push = jellyfin::Tally::default();
    let mut pull = jellyfin::PullTally::default();
    for dir in dirs {
        let dir =
            std::fs::canonicalize(dir).with_context(|| format!("reading {}", dir.display()))?;
        // An artist folder Jellyfin doesn't know yet is still an artist folder
        // (reported as not in Jellyfin), not a library to walk.
        let folders = if session.find(&dir, None).is_some() || jellyfin::is_artist_folder(&dir) {
            vec![dir]
        } else if action == JellyfinAction::Pull {
            // A folder with no images yet is just where a pull helps.
            jellyfin::subfolders(&dir)?
                .into_iter()
                .filter(|folder| session.find(folder, None).is_some())
                .collect()
        } else {
            jellyfin::artist_folders(&dir)?
        };
        for folder in folders {
            let label = folder_label(&folder);
            match action {
                JellyfinAction::Pull => {
                    if let Some(result) = session.pull_folder(&folder, None, &revisions, &opts)
                        && let Some(line) = pull.record(&label, result, dry_run)
                    {
                        out(line);
                    }
                }
                JellyfinAction::Push | JellyfinAction::Refresh => {
                    let result = if action == JellyfinAction::Refresh {
                        Some(session.rescan_folder(&folder, &opts))
                    } else {
                        session.sync_folder(&folder, None, &opts)
                    };
                    push.record(&label, result, dry_run)
                        .into_iter()
                        .for_each(&mut out);
                }
            }
        }
    }
    Ok(if action == JellyfinAction::Pull {
        (pull.summary(dry_run), pull.errors)
    } else {
        (push.summary(dry_run), push.errors)
    })
}

/// The subcommands, which work on the music folder directly (no state dir).
fn run_command(command: &cli::Command) -> Result<()> {
    match command {
        cli::Command::Skip { images, dirs } => skip_images(images, dirs),
        cli::Command::Keep { files, dirs } => keep_paths(files, dirs),
        cli::Command::Archive { files } => archive::run(files),
        cli::Command::Optimize {
            dirs,
            dry_run,
            verify,
        } => optimize::run(
            dirs,
            &optimize::Options {
                dry_run: *dry_run,
                verify: *verify,
            },
        ),
        cli::Command::Jellyfin { action } => jellyfin_command(action),
        cli::Command::Restore {
            dir,
            file,
            keep,
            state_dir,
            env_file: _,
            jellyfin,
            skip_jellyfin,
        } => {
            let target = match (dir, file) {
                (Some(dir), _) => restore::Target::Dir(dir),
                (None, Some(file)) => restore::Target::File(file),
                (None, None) => unreachable!("clap requires --dir or --file"),
            };
            if let Some(dir) = restore::run(&target, *keep)?
                && !skip_jellyfin
                && let Some(server) = jellyfin.server()
            {
                rescan_in_jellyfin(&dir, server, state_dir.as_ref());
            }
            Ok(())
        }
    }
}

/// `gatefold skip`: mark each image (`--file`), and every image in each folder
/// (`--dir`), as unwanted. Every image is attempted; the
/// command fails if any of them couldn't be marked.
fn skip_images(images: &[PathBuf], dirs: &[PathBuf]) -> Result<()> {
    let mut failed = 0;
    let mut all = images.to_vec();
    for dir in dirs {
        if let Err(err) = paths::require_dir(dir, Some("use --file to skip one image")) {
            failed += 1;
            eprintln!("✗ {err:#}");
            continue;
        }
        let found = skip::managed_in(dir);
        if found.is_empty() {
            println!("· {}: no images to skip", dir.display());
        }
        all.extend(found);
    }
    for image in &all {
        match skip::mark(image) {
            Ok(skip::Marked::Created(marker)) => println!(
                "✓ {}: skipped ({}); replaced once a different image is available",
                image.display(),
                marker.display()
            ),
            Ok(skip::Marked::AlreadySkipped(_)) => {
                println!("· {}: already skipped", image.display());
            }
            Err(err) => {
                failed += 1;
                eprintln!("✗ {err:#}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} path(s) could not be skipped");
    }
    Ok(())
}

/// `gatefold keep`: write the keep marker for each image (`--file`) and folder
/// (`--dir`). Every path is attempted; the command fails if any of them
/// couldn't be kept.
fn keep_paths(files: &[PathBuf], dirs: &[PathBuf]) -> Result<()> {
    let mut failed = 0;
    let marks = files
        .iter()
        .map(|path| (path, keep::mark_file(path)))
        .chain(dirs.iter().map(|path| (path, keep::mark_dir(path))));
    for (path, result) in marks {
        match result {
            Ok(kept) => {
                let marker = kept.marker.path().display();
                match kept.marker {
                    keep::Marked::Created(_) => println!("✓ {}: kept ({marker})", path.display()),
                    keep::Marked::AlreadyKept(_) => {
                        println!("· {}: already kept ({marker})", path.display());
                    }
                }
                if let Some(unskipped) = kept.unskipped {
                    println!("  skip taken back (removed {})", unskipped.display());
                }
                for marker in kept.replaced {
                    println!("  replaces {}", marker.display());
                }
            }
            Err(err) => {
                failed += 1;
                eprintln!("✗ {err:#}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} path(s) could not be kept");
    }
    Ok(())
}

/// Every remote service a run talks to, built once.
struct Services {
    lyrics: Client,
    cover: coverart::Client,
    musicbrainz: musicbrainz::Client,
    lookup: apple::Lookup,
    resolver: artist::Resolver,
    wikidata: wikidata::Client,
    /// Motion backdrops are wanted and ffmpeg is available to remux them.
    videos: bool,
}

impl Services {
    /// The API clients for this run. Motion backdrops need ffmpeg; without it
    /// they're turned off, with a note.
    fn new(
        cli: &Cli,
        musicbrainz: musicbrainz::Client,
        delay: Duration,
        progress: &Progress,
    ) -> Self {
        let videos = cli.artist_images_enabled() && !cli.skip_artist_videos;
        let ffmpeg = videos && video::ffmpeg_available();
        if videos && !ffmpeg {
            progress.info(
                "ffmpeg not found on PATH: motion backdrops (backdrops/backdrop.mp4) will be skipped",
            );
        }
        Self {
            lyrics: Client::new(
                cli.user_agent(),
                cli.app_id.clone(),
                cli.api_key.clone(),
                delay,
            ),
            cover: coverart::Client::new(cli.user_agent(), delay),
            musicbrainz,
            lookup: apple::Lookup::new(cli.user_agent(), delay),
            resolver: artist::Resolver::new(cli.user_agent(), delay),
            wikidata: wikidata::Client::new(cli.user_agent(), delay),
            videos: ffmpeg,
        }
    }
}

/// The cover cache key variant: bumped when the sources or sizes change so that
/// earlier caches are re-fetched rather than reused.
const COVER_CACHE_VARIANT: &str = "apple-1500|front-1200";

/// Lyrics and cover art, track by track. Also notes every album artist seen, for
/// the artist-images pass that follows.
fn run_track_pass(
    cli: &Cli,
    root: &Path,
    state: &State,
    (services, revisions): (&Services, &Revisions),
    progress: &Progress,
    tracks: &[Track],
    summary: &mut Summary,
) -> Result<ArtistJobs> {
    let mut cover_misses = CoverMemo::default();
    let mut artist_jobs = ArtistJobs::default();
    for (i, track) in tracks.iter().enumerate() {
        let name = track_name(track);
        progress.status(name.clone());

        let start = Instant::now();
        let result = process_track(
            cli,
            root,
            state,
            (services, revisions),
            progress,
            track,
            &name,
            summary,
            &mut cover_misses,
            &mut artist_jobs,
        );
        progress.record_track(start.elapsed());

        if let Err(err) = result {
            if err.downcast_ref::<Unauthorized>().is_some() {
                progress.finish();
                return Err(err.context("aborting: lrclib rejected the credentials"));
            }
            return Err(err);
        }

        // Log the running ETA once per folder — when the next track begins a new
        // folder — rather than interrupting a folder mid-way. Nothing is logged after
        // the final track, where no work remains and the ETA would just read ~0s.
        let folder_done_with_more = tracks
            .get(i + 1)
            .is_some_and(|next| next.path.parent() != track.path.parent());
        if folder_done_with_more {
            progress.log_progress();
        }
    }
    Ok(artist_jobs.finish())
}

/// The "Scanning" section: what was found, and what this run reads of it.
fn log_scan(
    progress: &Progress,
    root: &Path,
    tracks: &[Track],
    folders: Option<&[Vec<&Track>]>,
    to_read: usize,
) {
    progress.section("Scanning");
    progress.info(format!(
        "found {} music file(s) under {}",
        tracks.len(),
        root.display()
    ));
    if let Some(folders) = folders {
        progress.info(format!(
            "reading tags from {to_read} of them (up to {} per album folder, {} folder(s))",
            sample::PER_FOLDER,
            folders.len()
        ));
    } else {
        let with_lrc = tracks.iter().filter(|t| t.has_lrc).count();
        progress.info(format!(
            "{with_lrc} already have an adjacent .lrc, {} do not",
            tracks.len() - with_lrc
        ));
    }
}

/// `--apple-url`: remember the Apple Music artist id for the one album artist
/// found, so this and later runs use it over `MusicBrainz` and Wikidata.
fn pin_apple_id(
    state: &State,
    progress: &Progress,
    jobs: &ArtistJobs,
    apple_id: &str,
) -> Result<()> {
    let mut artists = jobs.by_id.values();
    let (Some(job), None) = (artists.next(), artists.next()) else {
        let names: Vec<&str> = jobs.by_id.values().map(|job| job.name.as_str()).collect();
        anyhow::bail!(
            "--apple-url needs exactly one album artist under --dir, found {}{}",
            names.len(),
            if names.is_empty() {
                String::new()
            } else {
                format!(": {}", names.join(", "))
            }
        );
    };
    state.save_apple_pin(&job.mbid, apple_id)?;
    progress.info(format!(
        "· {}: using Apple Music artist {apple_id}",
        job.name
    ));
    Ok(())
}

/// The track's `MusicBrainz` album artist id, the vote in [`run_artist_scan`].
fn id_of(meta: &TrackMetadata) -> Option<&str> {
    meta.musicbrainz_album_artist_id.as_deref()
}

/// The track's file name, as shown in progress and log lines.
fn track_name(track: &Track) -> String {
    track.path.file_name().map_or_else(
        || track.path.to_string_lossy().into_owned(),
        |n| pad_track_number(&n.to_string_lossy()),
    )
}

/// The artist-images-only stand-in for the track pass: read the sampled tracks
/// of each album folder (see [`sample`]) and note the album artist most of them
/// carry. No network here.
fn run_artist_scan(
    root: &Path,
    state: &State,
    progress: &Progress,
    folders: &[Vec<&Track>],
    summary: &mut Summary,
) -> Result<ArtistJobs> {
    let mut jobs = ArtistJobs::default();
    for folder in folders {
        let mut read = Vec::new();
        for &track in folder {
            let name = track_name(track);
            progress.status(name.clone());
            let start = Instant::now();
            let abs = std::fs::canonicalize(&track.path).unwrap_or_else(|_| track.path.clone());
            let digest = digest_for(&abs);
            let cached = state.load_metadata::<TrackMetadata>(&digest)?;
            match refresh_metadata(state, track, &digest, cached) {
                Ok(meta) => read.push((track, meta)),
                Err(err) => {
                    summary.unreadable += 1;
                    progress.info(format!("✗ {name}: {err:#}"));
                }
            }
            progress.record_track(start.elapsed());
        }
        let winner = sample::most_common(read.iter().filter_map(|(_, meta)| id_of(meta)));
        let pick = winner.and_then(|id| read.iter().find(|(_, meta)| id_of(meta) == Some(id)));
        if let Some((track, meta)) = pick {
            collect_artist(root, track, meta, &mut jobs);
        }
    }
    Ok(jobs.finish())
}

/// Fetch and write artist images for every distinct album artist seen during the
/// track loop. Runs as its own phase (with its own bar and ETA) once the tracks are
/// done, since artists are far fewer than tracks and need different requests.
#[allow(clippy::too_many_arguments)]
fn run_artist_pass(
    cli: &Cli,
    state: &State,
    services: &Services,
    revisions: &Revisions,
    progress: &Progress,
    delay: Duration,
    jobs: &ArtistJobs,
    summary: &mut Summary,
) {
    let mb = &services.musicbrainz;
    progress.section(&section_title(cli, "Artist images"));
    progress.info(format!(
        "{} distinct album artist(s) with a MusicBrainz id",
        jobs.by_id.len()
    ));
    for (folder, credit) in &jobs.joint {
        summary.artist_joint += 1;
        progress.info(joint_line(folder, credit, "no artist images"));
    }
    // Seed: one MusicBrainz lookup, a page fetch, up to two image downloads, plus RTT.
    let seed_secs = mb.delay().as_secs_f64() + delay.as_secs_f64() * 3.0 + 2.0;
    let bar = progress.phase("artists", "artist", jobs.by_id.len(), seed_secs);
    bar.log_initial_estimate();

    for (i, job) in jobs.by_id.values().enumerate() {
        bar.status(job.name.clone());
        let start = Instant::now();
        if !cli.dry_run {
            migrate_archives(&bar, job);
        }
        match process_artist(cli, state, services, revisions, &bar, job) {
            Ok(outcome) => {
                summary.record_artist(&outcome);
                log_artist(&bar, job, &outcome, cli.dry_run);
            }
            Err(err) => {
                summary.artist_errors += 1;
                bar.info(format!("✗ {}: {err:#}", job.name));
            }
        }
        bar.record_track(start.elapsed());
        if (i + 1) % 10 == 0 && i + 1 < jobs.by_id.len() {
            bar.log_progress();
        }
    }
    bar.finish();
}

/// Move archives an earlier version left next to the current files into
/// `.gatefold-archive/`, so Jellyfin stops indexing them. Failures are logged:
/// the artist's images are still processed.
fn migrate_archives(progress: &Progress, job: &ArtistJob) {
    for dir in &job.folders {
        match writer::migrate_archives(dir) {
            Ok(0) => {}
            Ok(moved) => progress.info(format!(
                "· {}: moved {moved} archived file(s) into {}/",
                job.name,
                writer::ARCHIVE_DIR
            )),
            Err(err) => progress.info(format!(
                "✗ {}: moving archives into {}/: {err:#}",
                job.name,
                writer::ARCHIVE_DIR
            )),
        }
    }
}

/// Load `--env-file` into the process environment before the main parse, so clap's
/// `env` fallbacks for the credentials and user-agent can read from it. The path is
/// scanned straight out of argv because clap resolves `env` during parsing — the file
/// must already be loaded by then. `dotenvy` does not overwrite variables already set
/// in the real environment, so precedence is: flag > real env var > env file. A read
/// failure exits with an error.
fn load_env_file() -> Result<()> {
    if let Some(path) = env_file_arg() {
        dotenvy::from_path(&path)
            .with_context(|| format!("loading env file {}", path.display()))?;
    }
    Ok(())
}

/// Scan argv for the last `--env-file <PATH>` / `--env-file=<PATH>` (last wins, as it
/// would with clap). Returns `None` if the flag is absent.
fn env_file_arg() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut found = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--env-file=") {
            found = Some(value.into());
        } else if arg == "--env-file" {
            found = args.next().map(Into::into);
        }
    }
    found
}

/// A one-line banner summarizing what this run will do, across every axis.
/// `mb_host` names a non-public `MusicBrainz` server so a misconfigured one is obvious.
fn run_summary(cli: &Cli, mb_host: Option<&str>) -> String {
    let mode = if cli.dry_run { "dry run" } else { "writing" };
    let scope = join_enabled(&[
        ("lyrics", cli.lyrics_enabled()),
        ("cover", cli.cover_enabled()),
        ("artist images", cli.artist_images_enabled()),
    ]);
    let select = if cli.new_only {
        "new-only"
    } else if cli.miss_only {
        "missed-only"
    } else {
        "all"
    };
    let select = match cli.retry_missed_after {
        Some(window) => format!(
            "{select} (misses within {} left alone)",
            cli::format_duration(window)
        ),
        None => select.to_string(),
    };
    let replace = join_enabled(&[
        ("lyrics", cli.replace_lyrics),
        ("cover", cli.replace_cover),
        ("artist images", cli.replace_artist_images),
    ]);
    let replace = match cli.artist_refresh_after {
        Some(window) => format!(
            "{replace} (artists checked within {} left alone)",
            cli::format_duration(window)
        ),
        None => replace,
    };
    let verify = if cli.verify { "on" } else { "off" };
    let mb = mb_host.map_or_else(String::new, |host| format!(" · musicbrainz: {host}"));
    format!(
        "gatefold · {mode} · scope: {scope} · select: {select} · replace: {replace} · verify: {verify}{mb}"
    )
}

/// A section title, marked when the run changes nothing.
fn section_title(cli: &Cli, title: &str) -> String {
    if cli.dry_run {
        format!("{title} (dry run)")
    } else {
        title.to_string()
    }
}

/// `"a + b"` for the enabled entries, or `"none"`.
fn join_enabled(parts: &[(&str, bool)]) -> String {
    let enabled: Vec<&str> = parts
        .iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect();
    if enabled.is_empty() {
        "none".to_string()
    } else {
        enabled.join(" + ")
    }
}

/// Resolve metadata (cached or freshly parsed) then run the lyrics and cover-art
/// sub-flows. Only a fatal lyrics error (`401`) propagates; per-track failures
/// are logged and counted.
#[allow(clippy::too_many_arguments)]
fn process_track(
    cli: &Cli,
    root: &Path,
    state: &State,
    (services, revisions): (&Services, &Revisions),
    progress: &Progress,
    track: &Track,
    name: &str,
    summary: &mut Summary,
    cover_misses: &mut CoverMemo,
    artist_jobs: &mut ArtistJobs,
) -> Result<()> {
    let abs = std::fs::canonicalize(&track.path).unwrap_or_else(|_| track.path.clone());
    let digest = digest_for(&abs);

    // Metadata cache: reuse an entry from the current schema whose mtime+size
    // still match the file, otherwise (missing, from an older build, or the file
    // was re-tagged/re-uploaded since) parse tags and persist. A file we can't
    // stat or lofty can't read is logged and skipped rather than aborting the run.
    let cached = state.load_metadata::<TrackMetadata>(&digest)?;
    let meta: TrackMetadata = match refresh_metadata(state, track, &digest, cached) {
        Ok(meta) => meta,
        Err(err) => {
            summary.unreadable += 1;
            progress.info(format!("✗ {name}: {err:#}"));
            return Ok(());
        }
    };

    // Previous tag values are snapshotted here before `--replace` overwrites them.
    let backup = Backup::new(state.backup_root(), &digest, &track.path);

    if cli.lyrics_enabled() {
        match process_lyrics(
            cli,
            state,
            &services.lyrics,
            progress,
            track,
            &meta,
            &digest,
            (&backup, revisions),
        ) {
            Ok(outcome) => {
                summary.record_lyrics(&outcome);
                log_lyrics(progress, name, &outcome, cli.dry_run);
            }
            Err(err) if err.downcast_ref::<Unauthorized>().is_some() => return Err(err),
            Err(err) => {
                summary.lyrics_errors += 1;
                progress.info(format!("✗ {name} (lyrics): {err:#}"));
            }
        }
    }

    if cli.cover_enabled() {
        match process_cover(
            cli,
            root,
            state,
            services,
            progress,
            track,
            &meta,
            cover_misses,
            &backup,
        ) {
            Ok(outcome) => {
                summary.record_cover(&outcome);
                log_cover(progress, name, &outcome, cli.dry_run);
            }
            Err(err) => {
                summary.cover_errors += 1;
                progress.info(format!("✗ {name} (cover): {err:#}"));
            }
        }
    }

    if cli.artist_images_enabled() {
        collect_artist(root, track, &meta, artist_jobs);
    }

    backup.finish()?;

    // Embedding lyrics or a cover rewrote the file, so the snapshot we started
    // from no longer matches it. Re-read now so the next run sees the tags as
    // they are instead of re-extracting them (or treating our own write as an
    // external change).
    if let Err(err) = refresh_metadata(state, track, &digest, Some(meta)) {
        progress.info(format!("✗ {name}: {err:#}"));
    }
    Ok(())
}

/// `cached` if it is from the current schema and the file's mtime+size haven't
/// changed since it was taken; otherwise re-extract the tags, persist the new
/// snapshot, and return it.
fn refresh_metadata(
    state: &State,
    track: &Track,
    digest: &str,
    cached: Option<TrackMetadata>,
) -> Result<TrackMetadata> {
    let fingerprint = Fingerprint::of(&track.path)?;
    if let Some(meta) = cached
        && meta.is_current(&fingerprint)
    {
        return Ok(meta);
    }
    let meta = metadata::extract(&track.path)?;
    state.save_metadata(digest, &meta)?;
    Ok(meta)
}

// ---- Lyrics -----------------------------------------------------------------

/// What happened to a track's lyrics.
enum LyricsOutcome {
    SkippedNothingToDo,
    SkippedMissingTags,
    SkippedByFilter,
    NotFound,
    NoLyrics,
    Applied {
        tag: writer::Write,
        lrc: writer::Write,
        reused: bool,
    },
}

#[allow(clippy::too_many_arguments)]
fn process_lyrics(
    cli: &Cli,
    state: &State,
    client: &Client,
    progress: &Progress,
    track: &Track,
    meta: &TrackMetadata,
    digest: &str,
    (backup, revisions): (&Backup, &Revisions),
) -> Result<LyricsOutcome> {
    let path = &track.path;
    // With --replace-lyrics we (re)write the tag even if one is already present.
    // An .lrc with no timestamps (plain lyrics) is retried for synced ones.
    let need_lrc = !track.has_lrc || writer::lrc_is_unsynced(&scanner::lrc_path_for(path));
    let need_tag = !meta.has_lyrics_tag || cli.replace_lyrics;
    if !need_lrc && !need_tag {
        return Ok(LyricsOutcome::SkippedNothingToDo);
    }
    if !meta.is_queryable() {
        return Ok(LyricsOutcome::SkippedMissingTags);
    }
    // --new-only: only brand-new items. --miss-only: only previously-recorded misses.
    if cli.new_only && state.has_lyrics_state(digest) {
        return Ok(LyricsOutcome::SkippedByFilter);
    }
    if cli.miss_only && !state.has_lyrics_miss(digest) {
        return Ok(LyricsOutcome::SkippedByFilter);
    }
    if miss_fresh(cli, state.lyrics_miss_age(digest)) {
        return Ok(LyricsOutcome::SkippedByFilter);
    }

    // Reuse a cached hit; otherwise (re)query. A recorded miss is deliberately
    // ignored here (--new-only and --retry-missed-after consult it above), so a
    // normal run rechecks it.
    let (record, reused) = if state.has_response(digest) {
        (state.load_response(digest)?, true)
    } else {
        fetch_lyrics(client, state, progress, meta, digest)?
    };

    let Some(record) = record else {
        return Ok(LyricsOutcome::NotFound);
    };

    if record.instrumental || (!record.has_plain() && !record.has_synced()) {
        return Ok(LyricsOutcome::NoLyrics);
    }

    let tag = if need_tag && record.has_plain() {
        writer::write_lyrics_tag(
            path,
            record.plain_lyrics.as_deref().unwrap(),
            cli.replace_lyrics,
            cli.verify,
            Some(backup),
            cli.dry_run,
        )?
    } else {
        writer::Write::Skipped
    };
    let lrc = if need_lrc && record.has_synced() {
        writer::write_lrc(
            path,
            record.synced_lyrics.as_deref().unwrap(),
            revisions,
            cli.dry_run,
        )?
    } else {
        writer::Write::Skipped
    };

    if tag.happened() || lrc.happened() {
        Ok(LyricsOutcome::Applied { tag, lrc, reused })
    } else {
        // A record existed, but nothing needed writing (lyrics already match, or
        // only a sidecar we already have) — not the same as lrclib having nothing.
        Ok(LyricsOutcome::SkippedNothingToDo)
    }
}

/// Fetch lyrics from lrclib, caching a success. Returns `(None, false)` for 404.
fn fetch_lyrics(
    client: &Client,
    state: &State,
    progress: &Progress,
    meta: &TrackMetadata,
    digest: &str,
) -> Result<(Option<LyricsRecord>, bool)> {
    let query = Query {
        track_name: meta.track_name.as_deref().unwrap(),
        artist_name: meta.artist_name.as_deref().unwrap(),
        album_name: meta.album_name.as_deref().filter(|s| !s.trim().is_empty()),
        duration: meta.duration,
    };

    match client.get(&query, progress)? {
        Fetched::Found(record) => {
            state.save_response(digest, &record)?;
            Ok((Some(record), false))
        }
        Fetched::NotFound => {
            state.save_lyrics_miss(digest)?;
            Ok((None, false))
        }
    }
}

// ---- Cover art --------------------------------------------------------------

/// Per-run cover lookups, shared across an album's tracks.
#[derive(Default)]
struct CoverMemo {
    /// Releases found to have no art this run, so they aren't re-queried per track.
    misses: HashSet<String>,
    /// Releases whose cached art was skipped and has been looked up again this
    /// run, for a different image.
    rechecked: HashSet<String>,
}

/// What happened to a track's cover art.
enum CoverOutcome {
    SkippedNoMbid,
    /// `--replace-cover`, but a `.gatefold-keep` / `.gatefold-cover-keep` marker
    /// covers the album and its cover is already there.
    Kept,
    /// The only cover available is one skipped in the album folder
    /// (`gatefold skip`).
    Rejected,
    SkippedNothingToDo,
    SkippedByFilter,
    NotFound,
    Applied {
        embedded: writer::Write,
        sidecar: writer::Write,
        reused: bool,
        source: String,
    },
}

#[allow(clippy::too_many_arguments)]
fn process_cover(
    cli: &Cli,
    root: &Path,
    state: &State,
    services: &Services,
    progress: &Progress,
    track: &Track,
    meta: &TrackMetadata,
    cover_misses: &mut CoverMemo,
    backup: &Backup,
) -> Result<CoverOutcome> {
    let path = &track.path;
    let Some(release) = meta
        .musicbrainz_release_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(CoverOutcome::SkippedNoMbid);
    };
    let group = meta
        .musicbrainz_release_group_id
        .as_deref()
        .filter(|s| !s.trim().is_empty());

    let folder = path.parent();
    // With --replace-cover we (re)write even when a cover is already present —
    // unless a keep marker covers the album: then a missing cover is still
    // written, but an existing one is never replaced. A skipped `cover.*` is up
    // for replacement on any run, keep marker or not.
    let kept = folder.is_some_and(|dir| keep::cover(dir, root));
    let skipped = folder.is_some_and(skip::cover);
    let replace = (cli.replace_cover && !kept) || skipped;
    let need_embed = !meta.has_cover_art || replace;
    let need_sidecar = folder.is_some_and(|dir| !writer::folder_has_cover(dir) || replace);
    if !need_embed && !need_sidecar {
        return Ok(if cli.replace_cover && kept {
            CoverOutcome::Kept
        } else {
            CoverOutcome::SkippedNothingToDo
        });
    }

    // The art cache is keyed by release id (plus the source/size variant, so a
    // change there re-fetches), shared across every track on the release — only
    // the first track of an album hits the network.
    let key = digest_str(&format!("{release}:{COVER_CACHE_VARIANT}"));

    // --new-only: only brand-new releases. --miss-only: only previously-recorded misses.
    if cli.new_only && state.has_cover_state(&key) {
        return Ok(CoverOutcome::SkippedByFilter);
    }
    if cli.miss_only && !state.has_cover_miss(&key) {
        return Ok(CoverOutcome::SkippedByFilter);
    }
    if miss_fresh(cli, state.cover_miss_age(&key)) {
        return Ok(CoverOutcome::SkippedByFilter);
    }

    // Reuse a cached hit; else a within-run miss (so a release without art isn't
    // re-queried for every track); else look it up. The persisted `.miss` marker is
    // deliberately ignored here (--new-only and --retry-missed-after consult it
    // above), so a normal run rechecks it.
    let (mut art, mut reused) = if state.has_art(&key) {
        (state.load_art(&key)?, true)
    } else if cover_misses.misses.contains(&key) {
        (None, true)
    } else {
        let fetched = fetch_cover(services, state, progress, &key, meta, release, group, false)?;
        if fetched.is_none() {
            cover_misses.misses.insert(key.clone());
        }
        (fetched, false)
    };

    // The cached cover was skipped here: look it up again (once per run), in case
    // a different one is available now. A miss keeps the cached image.
    let rejected = |art: &Option<state::CachedArt>| matches!((folder, art), (Some(dir), Some(art)) if skip::rejects_art(dir, art));
    if reused
        && rejected(&art)
        && cover_misses.rechecked.insert(key.clone())
        && let Some(fresh) =
            fetch_cover(services, state, progress, &key, meta, release, group, true)?
    {
        art = Some(fresh);
        reused = false;
    }
    if rejected(&art) {
        return Ok(CoverOutcome::Rejected);
    }

    let Some(art) = art else {
        return Ok(CoverOutcome::NotFound);
    };
    let source = art
        .source
        .clone()
        .unwrap_or_else(|| "cover art archive".to_string());

    let embedded = if need_embed {
        writer::embed_cover(path, &art, replace, cli.verify, Some(backup), cli.dry_run)?
    } else {
        writer::Write::Skipped
    };
    let sidecar = match folder {
        Some(dir) if need_sidecar => {
            writer::write_cover_sidecar(dir, &art, replace, Some(backup), cli.dry_run)?
        }
        _ => writer::Write::Skipped,
    };

    if embedded.happened() || sidecar.happened() {
        Ok(CoverOutcome::Applied {
            embedded,
            sidecar,
            reused,
            source,
        })
    } else {
        // Art was available but the file already had it — nothing to do.
        Ok(CoverOutcome::SkippedNothingToDo)
    }
}

/// Download the front cover for a release: Apple Music's artwork when the album
/// can be found there (larger than anything the archive has), else the Cover Art
/// Archive's front image for the release, falling back to its release group.
/// Caches and returns a success, `None` when nothing was found. `recheck`
/// bypasses the cached Apple Music lookups (a recorded miss always does).
/// A recheck that finds nothing records no miss: the cached art stays.
#[allow(clippy::too_many_arguments)]
fn fetch_cover(
    services: &Services,
    state: &State,
    progress: &Progress,
    key: &str,
    meta: &TrackMetadata,
    release: &str,
    group: Option<&str>,
    recheck: bool,
) -> Result<Option<state::CachedArt>> {
    let forced = recheck;
    let recheck = recheck || state.has_cover_miss(key);
    let apple = apple::resolve_release_art(
        state,
        &services.musicbrainz,
        &services.lookup,
        progress,
        release,
        group,
        meta.barcode.as_deref(),
        recheck,
    )?;
    let mut found = None;
    if let Some(art) = apple
        && let Some(image) = services.resolver.fetch_image(&art.artwork_url, progress)?
    {
        found = Some((image, "apple music"));
    }

    if found.is_none() {
        let client = &services.cover;
        let mut image = match client.front("release", release, progress)? {
            coverart::Fetched::Found(image) => Some(image),
            coverart::Fetched::NotFound => None,
        };
        if let (None, Some(group)) = (&image, group) {
            image = match client.front("release-group", group, progress)? {
                coverart::Fetched::Found(image) => Some(image),
                coverart::Fetched::NotFound => None,
            };
        }
        found = image.map(|image| (image, "cover art archive"));
    }

    if let Some((image, source)) = found {
        let art = state.save_art(key, &image.ext, &image.mime, Some(source), image.data)?;
        Ok(Some(art))
    } else {
        if !forced {
            state.save_cover_miss(key)?;
        }
        Ok(None)
    }
}

// ---- Artist images ----------------------------------------------------------

/// One album artist seen during the track loop, with every artist folder its
/// tracks resolved to (an artist split across two roots gets images in both).
#[derive(Debug)]
struct ArtistJob {
    mbid: String,
    name: String,
    folders: BTreeSet<PathBuf>,
    /// A track folder for this artist, named in the "no artist folder" log line.
    sample_dir: PathBuf,
}

/// Every album artist seen during the track loop, by `MusicBrainz` id, and the
/// artist folders named after a joint credit.
#[derive(Debug, Default)]
struct ArtistJobs {
    by_id: BTreeMap<String, ArtistJob>,
    /// Folders named after a credit of several artists ("A + B"), with that
    /// credit. They aren't one artist's, so they get no images, no
    /// `artist.nfo` and no Jellyfin push.
    joint: BTreeMap<PathBuf, String>,
}

impl ArtistJobs {
    /// Note `name` (`mbid`) as an album artist, seen in track folder `dir`, with
    /// its artist `folder` when there's one.
    fn add(&mut self, mbid: &str, name: &str, dir: &Path, folder: Option<PathBuf>) {
        let job = self
            .by_id
            .entry(mbid.to_string())
            .or_insert_with(|| ArtistJob {
                mbid: mbid.to_string(),
                name: name.to_string(),
                folders: BTreeSet::new(),
                sample_dir: dir.to_path_buf(),
            });
        job.folders.extend(folder);
    }

    /// Once every track is seen: no artist gets a joint credit's folder, even
    /// when a track tagged with just the credit and one artist's id led to it.
    fn finish(mut self) -> Self {
        for job in self.by_id.values_mut() {
            job.folders
                .retain(|folder| !self.joint.contains_key(folder));
        }
        self
    }
}

/// Note the track's album artist for the artist-images pass. No network here.
///
/// A credit of several artists (several `ALBUMARTISTS` or album artist ids) is
/// no one artist: each artist counts only through a folder named after that
/// artist (with the name and id paired as tagged), and a folder named after the
/// credit itself is noted as joint, so it never gets the first artist's images.
fn collect_artist(root: &Path, track: &Track, meta: &TrackMetadata, jobs: &mut ArtistJobs) {
    let Some(dir) = track.path.parent() else {
        return;
    };
    let names = &meta.release.album_artists;
    let ids = &meta.release.album_artist_ids;
    if names.len() > 1 || ids.len() > 1 {
        if let Some(credit) = meta.album_artist()
            && !names
                .iter()
                .any(|name| artist::normalize_name(name) == artist::normalize_name(credit))
            && let Some(folder) = artist::find_artist_folder(dir, root, credit)
        {
            jobs.joint.insert(folder, credit.to_string());
        }
        if names.len() == ids.len() {
            for (name, mbid) in names.iter().zip(ids) {
                if mbid == musicbrainz::VARIOUS_ARTISTS {
                    continue;
                }
                if let Some(folder) = artist::find_artist_folder(dir, root, name) {
                    jobs.add(mbid, name, dir, Some(folder));
                }
            }
        }
        return;
    }
    let Some(mbid) = meta.musicbrainz_album_artist_id.as_deref() else {
        return;
    };
    if mbid == musicbrainz::VARIOUS_ARTISTS {
        return;
    }
    let Some(name) = meta.album_artist() else {
        return;
    };
    jobs.add(mbid, name, dir, artist::find_artist_folder(dir, root, name));
}

/// What happened to an artist's images.
enum ArtistOutcome {
    NoFolder,
    /// `--replace-artist-images`, but every artist folder keeps all its images
    /// (`.gatefold-keep` & co) and has them already.
    Kept,
    /// `--replace-artist-images --artist-refresh-after`, but the artist's page was
    /// checked within the window, so it wasn't re-resolved.
    Fresh,
    SkippedNothingToDo,
    SkippedByFilter,
    /// Nothing to write, and only because Apple Music had nothing for the artist —
    /// no link (on `MusicBrainz` or Wikidata), or a link whose page has no artist
    /// photo: Spotify provides just an avatar, which is already present (or never
    /// overwrites one).
    SpotifyOnly {
        apple_link: bool,
    },
    NoLinks,
    NoImage,
    /// `--replace-artist-images`, but the images are the ones already written.
    Unchanged,
    Applied {
        avatar: writer::Write,
        backdrop: writer::Write,
        video: writer::Write,
        logo: writer::Write,
        source: Source,
        reused: bool,
        /// Where replaced files were moved, relative to the artist folder.
        kept: Vec<String>,
        /// Images not written but archived for `gatefold restore`.
        alternatives: Vec<String>,
        /// The artist has an Apple Music link (so a Spotify source means its
        /// page had no artist photo).
        apple_link: bool,
    },
}

#[allow(clippy::similar_names)] // `resolver` resolves, `resolved` is what it found
fn process_artist(
    cli: &Cli,
    state: &State,
    services: &Services,
    revisions: &Revisions,
    progress: &Progress,
    job: &ArtistJob,
) -> Result<ArtistOutcome> {
    let resolver = &services.resolver;
    let mbid = job.mbid.as_str();
    if job.folders.is_empty() {
        return Ok(ArtistOutcome::NoFolder);
    }

    let record: Option<ArtistRecord> = state
        .load_artist_record::<ArtistRecord>(mbid)?
        .filter(ArtistRecord::is_current);
    let now = revisions.stamp();
    let mode = replace_mode(cli, job, record.as_ref(), now);
    let replacing = mode == ReplaceMode::Replace;
    let wants_replace = replacing || mode == ReplaceMode::Kept;
    let look_again = wants_replace || retry_apple(state, mode, record.as_ref(), mbid)?;

    // Every folder already complete and not replacing (or retrying Apple):
    // nothing to do, and no reason to touch the network.
    if !look_again && folders_complete(job, record.as_ref(), services.videos) {
        return Ok(complete_outcome(mode, record.as_ref()));
    }

    if artist_filtered_out(cli, state, mbid) {
        return Ok(ArtistOutcome::SkippedByFilter);
    }
    // A recorded miss is rechecked from scratch (links included), like cover art.
    let rechecking = state.has_artist_miss(mbid);

    // 1. Links, via MusicBrainz, then Wikidata (both cached per artist).
    let Some(links) = artist_links(state, services, progress, job, rechecking)? else {
        state.save_artist_miss(mbid)?;
        return Ok(ArtistOutcome::NoLinks);
    };

    // 2. Image URLs, from the artist's page. A replace run always re-resolves so
    //    a changed photo (which changes the URL) is noticed.
    let reuse = record.as_ref().filter(|_| !look_again && !rechecking);
    let (resolved, reused): (Resolved, bool) = if let Some(record) = reuse {
        (record.resolved(), true)
    } else if let Some(resolved) = resolver.resolve(&links, progress)? {
        (resolved, false)
    } else {
        // Don't turn a previously-good artist into a miss on a replace run.
        if record.is_none() {
            state.save_artist_miss(mbid)?;
        }
        return Ok(ArtistOutcome::NoImage);
    };

    // Churn guard: only overwrite when the images differ from what was written.
    // Spotify only ever provides an avatar, and a lower-quality one than Apple's —
    // so never let a Spotify fall-through overwrite an existing file. A record
    // that was never written (e.g. from a --dry-run) says nothing about what's
    // on disk, so it can't vouch that the images are already there. Nor can any
    // record under --apple-url, which may be swapping in a different artist's
    // page; the writer still compares each file's digest before replacing it.
    let replace = replacing
        && resolved.source != Source::Spotify
        && (cli.apple_url.is_some()
            || record
                .as_ref()
                .is_none_or(|r| r.written_at.is_none() || !r.same_images(&resolved)));
    // A fresh page check is worth remembering even when nothing changed, so
    // --artist-refresh-after can count from it.
    let mut new_record = match record.clone().filter(|r| r.same_images(&resolved)) {
        Some(record) if reused => record,
        Some(record) => record.rechecked(now),
        None => ArtistRecord::new(mbid, &job.name, &resolved, now),
    };
    let record_changed = record.is_none() || !reused;
    let apple_link = links.apple_id.is_some();
    let nothing_to_do = nothing_to_do(resolved.source, mode, apple_link);
    // Apple images a keep marker turns away on a replace run are archived, so
    // `gatefold restore` can bring them in (Spotify's avatar isn't worth it).
    let alternatives = wants_replace && resolved.source == Source::Apple;

    let want_video = services.videos && resolved.video_url.is_some();
    if !missing_files(job, &resolved, replace, alternatives, want_video) {
        if record_changed {
            state.save_artist_record(mbid, &new_record)?;
        }
        return Ok(nothing_to_do);
    }

    // 3. Bytes (cached by image URL, so a changed photo is a fresh download).
    let Some(images) = artist_images(state, resolver, progress, job, &resolved)? else {
        if record.is_none() {
            state.save_artist_miss(mbid)?;
        }
        return Ok(ArtistOutcome::NoImage);
    };

    let video = match (&resolved.video_url, want_video) {
        (Some(url), true) => artist_video(state, resolver, progress, job, url)?,
        _ => None,
    };

    // 4. Write into every artist folder.
    let writes = (replace, alternatives);
    let written =
        write_artist_images(job, &images, video.as_ref(), writes, revisions, cli.dry_run)?;
    if written.wrote() || !written.alternatives.is_empty() {
        // A dry run wrote nothing: the record still caches what was resolved.
        if written.wrote() && !cli.dry_run {
            new_record.written_at = Some(now);
        }
        state.save_artist_record(mbid, &new_record)?;
        Ok(written.into_outcome(resolved.source, reused, apple_link))
    } else {
        if record_changed {
            state.save_artist_record(mbid, &new_record)?;
        }
        Ok(nothing_to_do)
    }
}

/// What an artist that needed no writes amounts to.
fn nothing_to_do(source: Source, mode: ReplaceMode, apple_link: bool) -> ArtistOutcome {
    if mode == ReplaceMode::Kept {
        ArtistOutcome::Kept
    } else if source == Source::Spotify {
        ArtistOutcome::SpotifyOnly { apple_link }
    } else if mode == ReplaceMode::Replace {
        ArtistOutcome::Unchanged
    } else {
        ArtistOutcome::SkippedNothingToDo
    }
}

/// What an artist whose folders already have every image amounts to.
fn complete_outcome(mode: ReplaceMode, record: Option<&ArtistRecord>) -> ArtistOutcome {
    match mode {
        ReplaceMode::Kept => ArtistOutcome::Kept,
        ReplaceMode::Fresh => ArtistOutcome::Fresh,
        ReplaceMode::Plain | ReplaceMode::Replace => {
            // Never a Spotify record with a cached Apple link: that one is
            // retried instead (see [`retry_apple`]).
            if record.is_some_and(|r| r.source == Source::Spotify) {
                ArtistOutcome::SpotifyOnly { apple_link: false }
            } else {
                ArtistOutcome::SkippedNothingToDo
            }
        }
    }
}

/// `--new-only`: only artists never looked up. `--miss-only`: only recorded misses.
/// `--retry-missed-after`: not a miss recorded within the window.
fn artist_filtered_out(cli: &Cli, state: &State, mbid: &str) -> bool {
    (cli.new_only && state.has_artist_state(mbid))
        || (cli.miss_only && !state.has_artist_miss(mbid))
        || miss_fresh(cli, state.artist_miss_age(mbid))
}

/// True when `--retry-missed-after` is set and this miss was recorded within its window.
fn miss_fresh(cli: &Cli, miss_age: Option<Duration>) -> bool {
    matches!((cli.retry_missed_after, miss_age), (Some(window), Some(age)) if age < window)
}

/// Whether this artist's existing images are up for replacement on this run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplaceMode {
    /// No `--replace-artist-images`: only missing files are written.
    Plain,
    Replace,
    /// Every folder keeps all its images (`.gatefold-keep` & co). Still resolved,
    /// so Apple's current images can be archived as alternatives.
    Kept,
    /// Checked within the `--artist-refresh-after` window.
    Fresh,
}

/// Keep markers in every folder turn a replace run into a plain one: missing
/// files are still written, existing ones are left alone (Apple's differing
/// ones are archived instead). A record resolved within the
/// `--artist-refresh-after` window isn't resolved again at all.
fn replace_mode(
    cli: &Cli,
    job: &ArtistJob,
    record: Option<&ArtistRecord>,
    now: u64,
) -> ReplaceMode {
    // --apple-url names the artist's page by hand: always use it, this run.
    if cli.apple_url.is_some() {
        return if fully_kept(job) {
            ReplaceMode::Kept
        } else {
            ReplaceMode::Replace
        };
    }
    if !cli.replace_artist_images {
        return ReplaceMode::Plain;
    }
    let fresh = match (cli.artist_refresh_after, record) {
        (Some(window), Some(record)) => now.saturating_sub(record.resolved_at) < window.as_secs(),
        _ => false,
    };
    if fresh {
        ReplaceMode::Fresh
    } else if fully_kept(job) {
        ReplaceMode::Kept
    } else {
        ReplaceMode::Replace
    }
}

/// True when every artist folder keeps all its artist images (`keep` markers), so
/// a replace run has nothing to replace.
fn fully_kept(job: &ArtistJob) -> bool {
    !job.folders.is_empty() && job.folders.iter().all(|dir| keep::all_artist_images(dir))
}

/// True when some artist folder lacks a file the resolution can provide (a
/// skipped file counts as lacking), a replace is due for a file no keep marker
/// protects, or (with `alternatives`) a kept file may have an alternative to
/// archive.
fn missing_files(
    job: &ArtistJob,
    resolved: &Resolved,
    replace: bool,
    alternatives: bool,
    want_video: bool,
) -> bool {
    job.folders.iter().any(|dir| {
        let needs = |kind: ArtistImage, wanted: bool| {
            let kept = keep::artist_image(dir, kind);
            wanted
                && (skip::present(dir, kind).is_none()
                    || (replace && !kept)
                    || (alternatives && kept))
        };
        needs(ArtistImage::Avatar, true)
            || needs(ArtistImage::Backdrop, resolved.backdrop_url.is_some())
            || needs(ArtistImage::Video, want_video)
            || needs(ArtistImage::Logo, resolved.logo_url.is_some())
    })
}

/// True when every artist folder already has the files the source can provide
/// (a Spotify-sourced artist has no backdrop to wait for; a video only counts
/// when the artist is known to have one and ffmpeg is available; a logo only
/// when the artist is known to have one). A skipped file doesn't count.
fn folders_complete(job: &ArtistJob, record: Option<&ArtistRecord>, videos: bool) -> bool {
    let has_backdrop_source = record.is_none_or(|r| r.backdrop_url.is_some());
    let has_video_source = videos && record.is_none_or(|r| r.video_url.is_some());
    let has_logo_source = record.is_none_or(|r| r.logo_url.is_some());
    job.folders.iter().all(|dir| {
        let has = |kind| skip::present(dir, kind).is_some();
        has(ArtistImage::Avatar)
            && (!has_backdrop_source || has(ArtistImage::Backdrop))
            && (!has_video_source || has(ArtistImage::Video))
            && (!has_logo_source || has(ArtistImage::Logo))
    })
}

/// A Spotify record for an artist with an Apple Music link means Apple's page
/// gave no photo last time (a hiccup, or an older gatefold rejecting it), so a
/// plain run looks again rather than settle for Spotify's avatar for good.
fn retry_apple(
    state: &State,
    mode: ReplaceMode,
    record: Option<&ArtistRecord>,
    mbid: &str,
) -> Result<bool> {
    Ok(mode == ReplaceMode::Plain
        && record.is_some_and(|r| r.source == Source::Spotify)
        && cached_apple_link(state, mbid)?)
}

/// Whether the artist is already known (from cache alone, no network) to have an
/// Apple Music link: pinned with --apple-url, or on its cached `MusicBrainz` or
/// Wikidata lookup.
fn cached_apple_link(state: &State, mbid: &str) -> Result<bool> {
    if state.load_apple_pin(mbid)?.is_some() {
        return Ok(true);
    }
    if let Some(lookup) = state.load_musicbrainz_artist(mbid)?
        && musicbrainz::links_from(&lookup).apple_id.is_some()
    {
        return Ok(true);
    }
    Ok(state
        .load_wikidata_artist(mbid)?
        .is_some_and(|result| wikidata::links_from(&result).apple_id.is_some()))
}

/// The artist's streaming links from its `MusicBrainz` lookup, topped up from
/// its Wikidata item when `MusicBrainz` has no Apple Music link (both cached per
/// artist). `None` when the artist is unknown or has no Apple Music / Spotify
/// link anywhere.
fn artist_links(
    state: &State,
    services: &Services,
    progress: &Progress,
    job: &ArtistJob,
    rechecking: bool,
) -> Result<Option<musicbrainz::ArtistLinks>> {
    let mbid = job.mbid.as_str();
    let cached = if rechecking {
        None
    } else {
        state.load_musicbrainz_artist(mbid)?
    };
    let lookup = if let Some(value) = cached {
        value
    } else {
        let Some(value) = services.musicbrainz.artist(mbid, progress)? else {
            return Ok(None);
        };
        state.save_musicbrainz_artist(mbid, &value)?;
        value
    };
    let mut links = musicbrainz::links_from(&lookup);
    if let Some(apple_id) = state.load_apple_pin(mbid)? {
        links.apple_id = Some(apple_id);
    }
    if links.apple_id.is_none() {
        let cached = if rechecking {
            None
        } else {
            state.load_wikidata_artist(mbid)?
        };
        let result = if let Some(value) = cached {
            Some(value)
        } else {
            let qid = musicbrainz::wikidata_id(&lookup);
            let value = services
                .wikidata
                .artist_links(mbid, qid.as_deref(), progress)?;
            if let Some(value) = &value {
                state.save_wikidata_artist(mbid, value)?;
            }
            value
        };
        if let Some(result) = result
            && links.fill_from(wikidata::links_from(&result))
        {
            progress.info(format!("· {}: streaming link found on Wikidata", job.name));
        }
    }
    Ok((!links.is_empty()).then_some(links))
}

/// What writing an artist's files across its folders amounted to.
struct Written {
    avatar: writer::Write,
    backdrop: writer::Write,
    video: writer::Write,
    logo: writer::Write,
    /// Where replaced files were moved, relative to the artist folder.
    kept: Vec<String>,
    /// Downloaded images not written (skipped, or kept out by a keep marker)
    /// but archived for `gatefold restore`, relative to the artist folder.
    alternatives: Vec<String>,
}

impl Written {
    /// True when any file was created or replaced.
    fn wrote(&self) -> bool {
        [self.avatar, self.backdrop, self.video, self.logo]
            .iter()
            .any(|w| w.happened())
    }

    fn into_outcome(self, source: Source, reused: bool, apple_link: bool) -> ArtistOutcome {
        ArtistOutcome::Applied {
            avatar: self.avatar,
            backdrop: self.backdrop,
            video: self.video,
            logo: self.logo,
            source,
            reused,
            kept: self.kept,
            alternatives: self.alternatives,
            apple_link,
        }
    }
}

/// Write the avatar (and backdrop, video and logo, when there are any) into
/// every folder of the job. `writes` is `(replace, alternatives)`. A replace
/// moves the previous file into its archive folder (`.gatefold-archive/primary/`,
/// `…/backdrops/`, `…/logos/`) first. A skipped file is replaced whatever the
/// mode. An image skipped in a folder is never written there, but archived; so
/// is one a keep marker turns away, with `alternatives`.
fn write_artist_images(
    job: &ArtistJob,
    images: &ArtistImages,
    video: Option<&state::CachedArt>,
    (replace, alternatives): (bool, bool),
    revisions: &Revisions,
    dry_run: bool,
) -> Result<Written> {
    let mut written = Written {
        avatar: writer::Write::Skipped,
        backdrop: writer::Write::Skipped,
        video: writer::Write::Skipped,
        logo: writer::Write::Skipped,
        kept: Vec::new(),
        alternatives: Vec::new(),
    };
    let files = [
        (ArtistImage::Avatar, Some(&images.avatar)),
        (ArtistImage::Backdrop, images.backdrop.as_ref()),
        (ArtistImage::Video, video),
        (ArtistImage::Logo, images.logo.as_ref()),
    ];
    for dir in &job.folders {
        for (kind, art) in &files {
            let Some(art) = art else { continue };
            let kept = keep::artist_image(dir, *kind);
            let turned_away = skip::rejects_art(&kind.target_dir(dir), art)
                || (alternatives && kept && skip::present(dir, *kind).is_some());
            if turned_away {
                let saved = writer::archive_alternative(dir, *kind, art, revisions, dry_run)?;
                written.alternatives.extend(saved);
                continue;
            }
            // Hand-curated in this folder: written if missing, never replaced.
            let replace = (replace && !kept) || skip::artist_image(dir, *kind);
            let (wrote, kept) =
                writer::write_artist_image(dir, *kind, art, replace, revisions, dry_run)?;
            let slot = match kind {
                ArtistImage::Avatar => &mut written.avatar,
                ArtistImage::Backdrop => &mut written.backdrop,
                ArtistImage::Video => &mut written.video,
                ArtistImage::Logo => &mut written.logo,
            };
            *slot = strongest(*slot, wrote);
            written.kept.extend(kept);
        }
    }
    Ok(written)
}

/// The still images fetched for an artist.
struct ArtistImages {
    avatar: state::CachedArt,
    backdrop: Option<state::CachedArt>,
    logo: Option<state::CachedArt>,
}

/// The avatar and (when the source has them) backdrop and logo bytes for a
/// resolution. `None` when the avatar is unavailable; a missing backdrop or logo
/// is logged and skipped.
fn artist_images(
    state: &State,
    resolver: &artist::Resolver,
    progress: &Progress,
    job: &ArtistJob,
    urls: &Resolved,
) -> Result<Option<ArtistImages>> {
    let Some(avatar) = artist_image(state, resolver, progress, &urls.avatar_url)? else {
        return Ok(None);
    };
    let optional = |url: &Option<String>, what: &str| -> Result<Option<state::CachedArt>> {
        let Some(url) = url else { return Ok(None) };
        let image = artist_image(state, resolver, progress, url)?;
        if image.is_none() {
            progress.info(format!("· {}: {what} unavailable, skipping it", job.name));
        }
        Ok(image)
    };
    let backdrop = optional(&urls.backdrop_url, "backdrop")?;
    let logo = optional(&urls.logo_url, "logo")?;
    Ok(Some(ArtistImages {
        avatar,
        backdrop,
        logo,
    }))
}

/// An artist's motion backdrop: from the art cache, else the best fitting HLS
/// variant written to MP4 by ffmpeg and cached. `None` (logged) when the
/// playlist is gone or ffmpeg fails, so the still images are still written.
fn artist_video(
    state: &State,
    resolver: &artist::Resolver,
    progress: &Progress,
    job: &ArtistJob,
    master_url: &str,
) -> Result<Option<state::CachedArt>> {
    let key = digest_str(master_url);
    if state.has_art(&key) {
        return state.load_art(&key);
    }
    let remuxed = (|| -> Result<Option<Vec<u8>>> {
        let Some(master) = resolver.fetch_text(master_url, progress)? else {
            return Ok(None);
        };
        let variants = video::variants(&master, master_url);
        let Some(variant) = video::pick(&variants) else {
            return Ok(None);
        };
        let work = std::env::temp_dir().join(format!("gatefold-{key}.mp4"));
        let result = video::remux(variant, resolver.user_agent(), &work)
            .and_then(|()| std::fs::read(&work).context("reading remuxed video"));
        let _ = std::fs::remove_file(&work);
        result.map(Some)
    })();
    match remuxed {
        Ok(Some(data)) => {
            let art = state.save_art(&key, "mp4", "video/mp4", Some("apple music"), data)?;
            Ok(Some(art))
        }
        Ok(None) => {
            progress.info(format!("· {}: motion backdrop unavailable", job.name));
            Ok(None)
        }
        Err(err) => {
            progress.info(format!("· {}: motion backdrop skipped: {err:#}", job.name));
            Ok(None)
        }
    }
}

/// An artist image by URL: from the art cache, else downloaded and cached.
fn artist_image(
    state: &State,
    resolver: &artist::Resolver,
    progress: &Progress,
    url: &str,
) -> Result<Option<state::CachedArt>> {
    let key = digest_str(url);
    if state.has_art(&key) {
        return state.load_art(&key);
    }
    match resolver.fetch_image(url, progress)? {
        Some(image) => {
            let source = Some("apple music");
            let art = state.save_art(&key, &image.ext, &image.mime, source, image.data)?;
            Ok(Some(art))
        }
        None => Ok(None),
    }
}

/// Across several folders, report the most significant thing that happened.
fn strongest(a: writer::Write, b: writer::Write) -> writer::Write {
    use writer::Write::{Created, Replaced, Skipped};
    match (a, b) {
        (Replaced, _) | (_, Replaced) => Replaced,
        (Created, _) | (_, Created) => Created,
        (Skipped, Skipped) => Skipped,
    }
}

// ---- Logging & summary ------------------------------------------------------

/// Right-align a leading `<digits>.` track number to 3 columns so track titles
/// line up in the log (` 1.` … `10.` … `100.`). Names without a numeric prefix
/// are returned unchanged.
fn pad_track_number(name: &str) -> String {
    match name.split_once('.') {
        Some((num, rest)) if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{num:>3}.{rest}")
        }
        _ => name.to_string(),
    }
}

fn log_lyrics(progress: &Progress, name: &str, outcome: &LyricsOutcome, dry_run: bool) {
    match outcome {
        LyricsOutcome::SkippedNothingToDo => {
            progress.info(format!("· {name}: lyrics already present"));
        }
        LyricsOutcome::SkippedMissingTags => {
            progress.info(format!("· {name}: lyrics skip (missing artist/title tags)"));
        }
        // Skipped by --new-only/--miss-only selection — stay quiet.
        LyricsOutcome::SkippedByFilter => {}
        LyricsOutcome::NotFound => progress.info(format!("· {name}: lyrics not found on lrclib")),
        LyricsOutcome::NoLyrics => progress.info(format!("· {name}: no lyrics available")),
        LyricsOutcome::Applied { tag, lrc, reused } => {
            let mut parts = Vec::new();
            match tag {
                writer::Write::Created => {
                    parts.push(format!("{} lyrics tag", act(dry_run, "wrote", "write")));
                }
                writer::Write::Replaced => parts.push(format!(
                    "{} lyrics tag",
                    act(dry_run, "replaced", "replace")
                )),
                writer::Write::Skipped => {}
            }
            match lrc {
                writer::Write::Created => {
                    parts.push(format!("{} .lrc file", act(dry_run, "wrote", "write")));
                }
                writer::Write::Replaced => parts.push(format!(
                    "{} unsynced .lrc file",
                    act(dry_run, "replaced", "replace")
                )),
                writer::Write::Skipped => {}
            }
            let src = if *reused { " (from cache)" } else { "" };
            progress.info(format!("✓ {name}: {}{src}", parts.join(", ")));
        }
    }
}

fn log_cover(progress: &Progress, name: &str, outcome: &CoverOutcome, dry_run: bool) {
    match outcome {
        // Missing MBID, "nothing to do", and filter skips are common; keep them quiet.
        // Kept covers would log once per track of the album; the summary counts them.
        CoverOutcome::SkippedNoMbid
        | CoverOutcome::SkippedNothingToDo
        | CoverOutcome::SkippedByFilter
        | CoverOutcome::Kept
        | CoverOutcome::Rejected => {}
        CoverOutcome::NotFound => {
            progress.info(format!("· {name}: cover art not found"));
        }
        CoverOutcome::Applied {
            embedded,
            sidecar,
            reused,
            source,
        } => {
            let mut parts = Vec::new();
            match embedded {
                writer::Write::Created => {
                    parts.push(format!("{} cover", act(dry_run, "embedded", "embed")));
                }
                writer::Write::Replaced => {
                    parts.push(format!(
                        "{} embedded cover",
                        act(dry_run, "replaced", "replace")
                    ));
                }
                writer::Write::Skipped => {}
            }
            match sidecar {
                writer::Write::Created => {
                    parts.push(format!("{} cover.jpg", act(dry_run, "wrote", "write")));
                }
                writer::Write::Replaced => {
                    parts.push(format!("{} cover.jpg", act(dry_run, "replaced", "replace")));
                }
                writer::Write::Skipped => {}
            }
            if parts.is_empty() {
                return;
            }
            let src = if *reused {
                format!("cache, {source}")
            } else {
                source.clone()
            };
            progress.info(format!("✓ {name}: {} ({src})", parts.join(", ")));
        }
    }
}

fn log_artist(progress: &Progress, job: &ArtistJob, outcome: &ArtistOutcome, dry_run: bool) {
    let name = &job.name;
    match outcome {
        ArtistOutcome::SkippedNothingToDo | ArtistOutcome::SkippedByFilter => {}
        ArtistOutcome::NoFolder => progress.info(format!(
            "· {name}: no artist folder above {}",
            job.sample_dir.display()
        )),
        ArtistOutcome::Kept => progress.info(format!("· {name}: artist images kept")),
        ArtistOutcome::Fresh => progress.info(format!("· {name}: artist page checked recently")),
        ArtistOutcome::SpotifyOnly { apple_link } => progress.info(format!(
            "· {name}: {}; Spotify only has an avatar, and {} is already there (set one with --apple-url)",
            no_apple_photo(*apple_link),
            ArtistImage::Avatar.label("jpg")
        )),
        ArtistOutcome::NoLinks => {
            progress.info(format!(
                "· {name}: no Apple Music/Spotify link on MusicBrainz or Wikidata"
            ));
        }
        ArtistOutcome::NoImage => {
            progress.info(format!("· {name}: no artist image on Apple Music/Spotify"));
        }
        ArtistOutcome::Unchanged => progress.info(format!("· {name}: artist images unchanged")),
        ArtistOutcome::Applied {
            avatar,
            backdrop,
            video,
            logo,
            source,
            reused,
            kept,
            alternatives,
            apple_link,
        } => {
            let mut parts = Vec::new();
            for (kind, ext, write) in [
                (ArtistImage::Avatar, "jpg", avatar),
                (ArtistImage::Backdrop, "jpg", backdrop),
                (ArtistImage::Video, "mp4", video),
                (ArtistImage::Logo, "png", logo),
            ] {
                let name = kind.label(ext);
                match write {
                    writer::Write::Created => parts.push(format!("{} {name}", act(dry_run, "wrote", "write"))),
                    writer::Write::Replaced => {
                        parts.push(format!("{} {name}", act(dry_run, "replaced", "replace")));
                    }
                    writer::Write::Skipped => {}
                }
            }
            if !alternatives.is_empty() {
                parts.push(format!(
                    "{} alternative {} (gatefold restore)",
                    act(dry_run, "archived", "archive"),
                    alternatives.join(", ")
                ));
            }
            let src = if *reused { "cache" } else { source.label() };
            let kept = if kept.is_empty() {
                String::new()
            } else {
                format!(", previous in {}", kept.join(", "))
            };
            let why = if *source == Source::Spotify {
                format!("; {}, so no backdrop or logo", no_apple_photo(*apple_link))
            } else {
                String::new()
            };
            progress.info(format!("✓ {name}: {} ({src}{kept}){why}", parts.join(", ")));
        }
    }
}

/// `done` ("wrote"), or on a dry run "would" + `todo` ("would write").
fn act(dry_run: bool, done: &str, todo: &str) -> String {
    if dry_run {
        format!("would {todo}")
    } else {
        done.to_string()
    }
}

/// Why a Spotify-sourced artist gets an avatar and nothing else.
fn no_apple_photo(apple_link: bool) -> &'static str {
    if apple_link {
        "no artist photo on its Apple Music page"
    } else {
        "no Apple Music link on MusicBrainz or Wikidata"
    }
}

/// Aggregate counts printed at the end of the run.
#[derive(Default)]
struct Summary {
    /// Files whose tags could not be read; nothing was done for them.
    unreadable: u64,
    // lyrics
    wrote_tag: u64,
    replaced_tag: u64,
    wrote_lrc: u64,
    /// Unsynced `.lrc` files replaced with synced ones (the old one archived).
    replaced_lrc: u64,
    lyrics_skipped: u64,
    lyrics_not_found: u64,
    no_lyrics: u64,
    lyrics_errors: u64,
    // cover art
    embedded_cover: u64,
    replaced_cover: u64,
    wrote_cover_sidecar: u64,
    replaced_cover_sidecar: u64,
    cover_skipped: u64,
    cover_kept: u64,
    cover_rejected: u64,
    cover_not_found: u64,
    cover_errors: u64,
    // artist images
    artist_wrote: u64,
    artist_replaced: u64,
    artist_unchanged: u64,
    artist_skipped: u64,
    artist_kept: u64,
    artist_fresh: u64,
    artist_no_folder: u64,
    /// Artist folders named after a joint credit ("A + B"), skipped.
    artist_joint: u64,
    artist_spotify_only: u64,
    artist_no_links: u64,
    artist_no_image: u64,
    artist_errors: u64,
    nfo: Option<NfoSummary>,
    // jellyfin
    jellyfin: Option<jellyfin::Tally>,
}

impl Summary {
    fn record_lyrics(&mut self, outcome: &LyricsOutcome) {
        match outcome {
            LyricsOutcome::SkippedNothingToDo
            | LyricsOutcome::SkippedMissingTags
            | LyricsOutcome::SkippedByFilter => {
                self.lyrics_skipped += 1;
            }
            LyricsOutcome::NotFound => self.lyrics_not_found += 1,
            LyricsOutcome::NoLyrics => self.no_lyrics += 1,
            LyricsOutcome::Applied { tag, lrc, .. } => {
                match tag {
                    writer::Write::Created => self.wrote_tag += 1,
                    writer::Write::Replaced => self.replaced_tag += 1,
                    writer::Write::Skipped => {}
                }
                match lrc {
                    writer::Write::Created => self.wrote_lrc += 1,
                    writer::Write::Replaced => self.replaced_lrc += 1,
                    writer::Write::Skipped => {}
                }
            }
        }
    }

    fn record_cover(&mut self, outcome: &CoverOutcome) {
        match outcome {
            CoverOutcome::SkippedNoMbid
            | CoverOutcome::SkippedNothingToDo
            | CoverOutcome::SkippedByFilter => {
                self.cover_skipped += 1;
            }
            CoverOutcome::NotFound => self.cover_not_found += 1,
            CoverOutcome::Kept => self.cover_kept += 1,
            CoverOutcome::Rejected => self.cover_rejected += 1,
            CoverOutcome::Applied {
                embedded, sidecar, ..
            } => {
                match embedded {
                    writer::Write::Created => self.embedded_cover += 1,
                    writer::Write::Replaced => self.replaced_cover += 1,
                    writer::Write::Skipped => {}
                }
                match sidecar {
                    writer::Write::Created => self.wrote_cover_sidecar += 1,
                    writer::Write::Replaced => self.replaced_cover_sidecar += 1,
                    writer::Write::Skipped => {}
                }
            }
        }
    }

    fn record_artist(&mut self, outcome: &ArtistOutcome) {
        match outcome {
            ArtistOutcome::SkippedNothingToDo | ArtistOutcome::SkippedByFilter => {
                self.artist_skipped += 1;
            }
            ArtistOutcome::NoFolder => self.artist_no_folder += 1,
            ArtistOutcome::Kept => self.artist_kept += 1,
            ArtistOutcome::Fresh => self.artist_fresh += 1,
            ArtistOutcome::SpotifyOnly { .. } => self.artist_spotify_only += 1,
            ArtistOutcome::NoLinks => self.artist_no_links += 1,
            ArtistOutcome::NoImage => self.artist_no_image += 1,
            ArtistOutcome::Unchanged => self.artist_unchanged += 1,
            ArtistOutcome::Applied {
                avatar,
                backdrop,
                video,
                logo,
                ..
            } => {
                if [avatar, backdrop, video, logo].contains(&&writer::Write::Replaced) {
                    self.artist_replaced += 1;
                } else {
                    self.artist_wrote += 1;
                }
            }
        }
    }

    fn print(&self, progress: &Progress, cli: &Cli) {
        if self.unreadable > 0 {
            progress.info(format!(
                "unreadable: {} file(s) could not be parsed and were skipped",
                self.unreadable
            ));
        }
        if cli.lyrics_enabled() {
            self.print_lyrics(progress, cli.dry_run);
        }
        if cli.cover_enabled() {
            self.print_cover(progress, cli.dry_run);
        }
        if cli.artist_images_enabled() {
            self.print_artist(progress, cli.dry_run);
        }
        if let Some(nfo) = &self.nfo {
            progress.info("nfo:");
            let summary = nfo.artists.summary(nfo::ARTIST_FILE, cli.dry_run);
            progress.info(format!("  {summary}"));
            let summary = nfo.albums.summary(nfo::ALBUM_FILE, cli.dry_run);
            progress.info(format!("  {summary}"));
        }
        if let Some(tally) = &self.jellyfin {
            progress.info("jellyfin:");
            progress.info(format!("  {}", tally.summary(cli.dry_run)));
        }
    }

    fn print_artist(&self, progress: &Progress, d: bool) {
        progress.info("artist images:");
        progress.info(format!(
            "  {} images for {} artist(s), {} {}",
            act(d, "wrote", "write"),
            self.artist_wrote,
            act(d, "replaced", "replace"),
            self.artist_replaced
        ));
        progress.info(format!(
            "  unchanged {}, skipped {}, kept {}, fresh {}, no folder {}, joint credit skipped {}, no apple music link {}, no links {}, no image {}, errors {}",
            self.artist_unchanged,
            self.artist_skipped,
            self.artist_kept,
            self.artist_fresh,
            self.artist_no_folder,
            self.artist_joint,
            self.artist_spotify_only,
            self.artist_no_links,
            self.artist_no_image,
            self.artist_errors
        ));
    }

    fn print_lyrics(&self, progress: &Progress, d: bool) {
        progress.info("lyrics:");
        progress.info(format!(
            "  {} {} tag(s), {} {}",
            act(d, "wrote", "write"),
            self.wrote_tag,
            act(d, "replaced", "replace"),
            self.replaced_tag
        ));
        progress.info(format!(
            "  {} {} .lrc file(s), {} {} unsynced",
            act(d, "created", "create"),
            self.wrote_lrc,
            act(d, "replaced", "replace"),
            self.replaced_lrc
        ));
        progress.info(format!(
            "  skipped {}, not found {}, none {}, errors {}",
            self.lyrics_skipped, self.lyrics_not_found, self.no_lyrics, self.lyrics_errors
        ));
    }

    fn print_cover(&self, progress: &Progress, d: bool) {
        progress.info("cover art:");
        progress.info(format!(
            "  {} {} cover(s), {} {}",
            act(d, "embedded", "embed"),
            self.embedded_cover,
            act(d, "replaced", "replace"),
            self.replaced_cover
        ));
        progress.info(format!(
            "  {} {} cover.jpg sidecar(s), {} {}",
            act(d, "wrote", "write"),
            self.wrote_cover_sidecar,
            act(d, "replaced", "replace"),
            self.replaced_cover_sidecar
        ));
        progress.info(format!(
            "  skipped {}, kept {}, rejected {}, not found {}, errors {}",
            self.cover_skipped,
            self.cover_kept,
            self.cover_rejected,
            self.cover_not_found,
            self.cover_errors
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn art(data: &[u8]) -> state::CachedArt {
        state::CachedArt::new("jpg", data)
    }

    fn job(dir: &Path) -> ArtistJob {
        ArtistJob {
            mbid: "mbid".to_string(),
            name: "Artist".to_string(),
            folders: BTreeSet::from([dir.to_path_buf()]),
            sample_dir: dir.to_path_buf(),
        }
    }

    /// A library with Yellowcard's own album and the Yellowcard + Hammock split
    /// both under `Yellowcard/`, and the split again under its own
    /// `Yellowcard + Hammock/` folder. The split's tracks come first, so the
    /// joint credit is the first name seen for Yellowcard's id.
    fn joint_library(name: &str) -> (PathBuf, Vec<(Track, TrackMetadata)>) {
        use nfo::tests::{YELLOWCARD, joint_track};
        let root = std::env::temp_dir().join(format!("gatefold-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut own = joint_track();
        own.album_artist_name = Some("Yellowcard".into());
        own.release.album_artist_values = vec!["Yellowcard".into()];
        own.release.album_artists = vec!["Yellowcard".into()];
        own.release.album_artist_ids = vec![YELLOWCARD.into()];
        let tracks = [
            ("Yellowcard/A Hopeful Sign (2024)", joint_track()),
            ("Yellowcard + Hammock/A Hopeful Sign (2024)", joint_track()),
            ("Yellowcard/Ocean Avenue (2003)", own),
        ];
        let tracks = tracks
            .into_iter()
            .map(|(album, meta)| {
                let dir = root.join(album);
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("01.flac");
                fs::write(&path, b"").unwrap();
                (
                    Track {
                        path,
                        has_lrc: false,
                    },
                    meta,
                )
            })
            .collect();
        (root, tracks)
    }

    #[test]
    fn a_joint_credit_never_names_or_takes_one_artist_folder() {
        use nfo::tests::YELLOWCARD;
        let (root, tracks) = joint_library("joint");
        let mut jobs = ArtistJobs::default();
        for (track, meta) in &tracks {
            collect_artist(&root, track, meta, &mut jobs);
        }
        let jobs = jobs.finish();

        let ids: Vec<&String> = jobs.by_id.keys().collect();
        assert_eq!(ids, [YELLOWCARD], "Hammock has no folder of its own");
        let yellowcard = &jobs.by_id[YELLOWCARD];
        assert_eq!(yellowcard.name, "Yellowcard");
        assert_eq!(
            yellowcard.folders,
            BTreeSet::from([root.join("Yellowcard")]),
            "never the joint folder, so it gets no Yellowcard images"
        );
        let joint = root.join("Yellowcard + Hammock");
        assert_eq!(
            jobs.joint,
            BTreeMap::from([(joint.clone(), "Yellowcard + Hammock".to_string())])
        );
        assert_eq!(
            joint_line(&joint, &jobs.joint[&joint], "no artist.nfo"),
            "· Yellowcard + Hammock: no artist.nfo, joint credit \"Yellowcard + Hammock\" (several artists), skipped"
        );

        // Yellowcard's artist.nfo stays Yellowcard's.
        let folder = root.join("Yellowcard");
        let name = nfo::artist_title("Yellowcard", None, &yellowcard.name, 1).unwrap();
        nfo::write_artist(&folder, name, &yellowcard.mbid, false).unwrap();
        let xml = fs::read_to_string(folder.join(nfo::ARTIST_FILE)).unwrap();
        assert!(xml.contains("<title>Yellowcard</title>"), "{xml}");
        assert!(xml.contains(&format!("<musicbrainzartistid>{YELLOWCARD}<")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_joint_folder_stays_joint_when_one_track_has_just_the_first_id() {
        use nfo::tests::YELLOWCARD;
        let (root, mut tracks) = joint_library("joint-degraded");
        // Another copy of the split, tagged with the credit and only
        // Yellowcard's id: on its own, it looks like Yellowcard's album.
        let mut meta = nfo::tests::joint_track();
        meta.release.album_artists.clear();
        meta.release.album_artist_ids = vec![YELLOWCARD.into()];
        let path = tracks[1].0.path.with_file_name("02.flac");
        tracks.push((
            Track {
                path,
                has_lrc: false,
            },
            meta,
        ));
        let mut jobs = ArtistJobs::default();
        for (track, meta) in &tracks {
            collect_artist(&root, track, meta, &mut jobs);
        }
        let jobs = jobs.finish();
        assert_eq!(
            jobs.by_id[YELLOWCARD].folders,
            BTreeSet::from([root.join("Yellowcard")])
        );
        assert!(jobs.joint.contains_key(&root.join("Yellowcard + Hammock")));
        fs::remove_dir_all(&root).unwrap();
    }

    fn avatar_only(data: &[u8]) -> ArtistImages {
        ArtistImages {
            avatar: art(data),
            backdrop: None,
            logo: None,
        }
    }

    fn archived(dir: &Path) -> Vec<Vec<u8>> {
        let archive = ArtistImage::Avatar.archive_dir(dir);
        let Ok(entries) = fs::read_dir(&archive) else {
            return Vec::new();
        };
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .map(|p| fs::read(p).unwrap())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn archives_images_that_markers_turn_away() {
        let root = std::env::temp_dir().join(format!("gatefold-alt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let revisions = Revisions::new();

        // Kept folder.jpg: a replace run leaves it and archives Apple's image.
        let kept = root.join("Kept");
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("folder.jpg"), b"mine").unwrap();
        fs::write(kept.join(keep::ALL), b"").unwrap();
        let written = write_artist_images(
            &job(&kept),
            &avatar_only(b"apple"),
            None,
            (false, true),
            &revisions,
            false,
        )
        .unwrap();
        assert_eq!(fs::read(kept.join("folder.jpg")).unwrap(), b"mine");
        assert_eq!(archived(&kept), vec![b"apple".to_vec()]);
        assert_eq!(written.alternatives.len(), 1);
        assert!(!written.wrote());
        // Again: already archived, nothing new.
        let again = write_artist_images(
            &job(&kept),
            &avatar_only(b"apple"),
            None,
            (false, true),
            &revisions,
            false,
        )
        .unwrap();
        assert_eq!(again.alternatives, Vec::<String>::new());
        // Apple's image being the kept one is no alternative.
        let same = write_artist_images(
            &job(&kept),
            &avatar_only(b"mine"),
            None,
            (false, true),
            &revisions,
            false,
        )
        .unwrap();
        assert_eq!(same.alternatives, Vec::<String>::new());

        // Not a replace run: a kept folder isn't touched at all.
        let plain = root.join("Plain");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("folder.jpg"), b"mine").unwrap();
        fs::write(plain.join(keep::ALL), b"").unwrap();
        let written = write_artist_images(
            &job(&plain),
            &avatar_only(b"apple"),
            None,
            (false, false),
            &revisions,
            false,
        )
        .unwrap();
        assert_eq!(written.alternatives, Vec::<String>::new());
        assert_eq!(archived(&plain), Vec::<Vec<u8>>::new());

        // A skipped image is never written, but archived, whatever the run.
        let skipped = root.join("Skipped");
        fs::create_dir_all(&skipped).unwrap();
        let marker = skip::marker_name(&state::digest_bytes(b"rejected"));
        fs::write(skipped.join(marker), b"").unwrap();
        let written = write_artist_images(
            &job(&skipped),
            &avatar_only(b"rejected"),
            None,
            (false, false),
            &revisions,
            false,
        )
        .unwrap();
        assert!(!skipped.join("folder.jpg").exists());
        assert_eq!(archived(&skipped), vec![b"rejected".to_vec()]);
        assert_eq!(written.alternatives.len(), 1);

        fs::remove_dir_all(&root).unwrap();
    }
}
