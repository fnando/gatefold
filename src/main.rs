mod apple;
mod archive;
mod artist;
mod backup;
mod cli;
mod coverart;
mod keep;
mod lossless;
mod lrclib;
mod metadata;
mod musicbrainz;
mod net;
mod optimize;
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
    let state_dir = match &cli.state_dir {
        Some(dir) => dir.clone(),
        None => state::default_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
            .context("no --state-dir given and neither $XDG_DATA_HOME nor $HOME is set")?,
    };
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
        progress.section(if cli.cache_only {
            "Processing (cache-only)"
        } else {
            "Processing"
        });
    }

    let mut summary = Summary::default();
    let artist_jobs = if let Some(folders) = &folders {
        run_artist_scan(&root, &state, &progress, folders, &mut summary)?
    } else {
        run_track_pass(
            &cli,
            &root,
            &state,
            &services,
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

    progress.section("Summary");
    summary.print(&progress, &cli);
    Ok(())
}

/// The subcommands, which work on the music folder directly (no state dir).
fn run_command(command: &cli::Command) -> Result<()> {
    match command {
        cli::Command::Skip { images } => skip_images(images),
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
        cli::Command::Restore { dir, file, keep } => {
            let target = match (dir, file) {
                (Some(dir), _) => restore::Target::Dir(dir),
                (None, Some(file)) => restore::Target::File(file),
                (None, None) => unreachable!("clap requires --dir or --file"),
            };
            restore::run(&target, *keep)
        }
    }
}

/// `gatefold skip`: mark each image as unwanted. Every image is attempted; the
/// command fails if any of them couldn't be marked.
fn skip_images(images: &[PathBuf]) -> Result<()> {
    let mut failed = 0;
    for image in images {
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
        anyhow::bail!("{failed} image(s) could not be skipped");
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
const COVER_CACHE_VARIANT: &str = "apple-4000|front-1200";

/// Lyrics and cover art, track by track. Also notes every album artist seen, for
/// the artist-images pass that follows.
fn run_track_pass(
    cli: &Cli,
    root: &Path,
    state: &State,
    services: &Services,
    progress: &Progress,
    tracks: &[Track],
    summary: &mut Summary,
) -> Result<BTreeMap<String, ArtistJob>> {
    let mut cover_misses = CoverMemo::default();
    let mut artist_jobs: BTreeMap<String, ArtistJob> = BTreeMap::new();
    for (i, track) in tracks.iter().enumerate() {
        let name = track_name(track);
        progress.status(name.clone());

        let start = Instant::now();
        let result = process_track(
            cli,
            root,
            state,
            services,
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
    Ok(artist_jobs)
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
    jobs: &BTreeMap<String, ArtistJob>,
    apple_id: &str,
) -> Result<()> {
    let mut artists = jobs.values();
    let (Some(job), None) = (artists.next(), artists.next()) else {
        let names: Vec<&str> = jobs.values().map(|job| job.name.as_str()).collect();
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
) -> Result<BTreeMap<String, ArtistJob>> {
    let mut jobs = BTreeMap::new();
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
    Ok(jobs)
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
    jobs: &BTreeMap<String, ArtistJob>,
    summary: &mut Summary,
) {
    let mb = &services.musicbrainz;
    progress.section(if cli.cache_only {
        "Artist images (cache-only)"
    } else {
        "Artist images"
    });
    progress.info(format!(
        "{} distinct album artist(s) with a MusicBrainz id",
        jobs.len()
    ));
    // Seed: one MusicBrainz lookup, a page fetch, up to two image downloads, plus RTT.
    let seed_secs = mb.delay().as_secs_f64() + delay.as_secs_f64() * 3.0 + 2.0;
    let bar = progress.phase("artists", "artist", jobs.len(), seed_secs);
    bar.log_initial_estimate();

    for (i, job) in jobs.values().enumerate() {
        bar.status(job.name.clone());
        let start = Instant::now();
        if !cli.cache_only {
            migrate_archives(&bar, job);
        }
        match process_artist(cli, state, services, revisions, &bar, job) {
            Ok(outcome) => {
                summary.record_artist(&outcome);
                log_artist(&bar, job, &outcome);
            }
            Err(err) => {
                summary.artist_errors += 1;
                bar.info(format!("✗ {}: {err:#}", job.name));
            }
        }
        bar.record_track(start.elapsed());
        if (i + 1) % 10 == 0 && i + 1 < jobs.len() {
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
    let mode = if cli.cache_only {
        "cache-only"
    } else {
        "writing"
    };
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
    services: &Services,
    progress: &Progress,
    track: &Track,
    name: &str,
    summary: &mut Summary,
    cover_misses: &mut CoverMemo,
    artist_jobs: &mut BTreeMap<String, ArtistJob>,
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
            &backup,
        ) {
            Ok(outcome) => {
                summary.record_lyrics(&outcome);
                log_lyrics(progress, name, &outcome);
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
                log_cover(progress, name, &outcome);
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
        lrc: bool,
        reused: bool,
    },
    CacheOnly {
        has_plain: bool,
        has_synced: bool,
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
    backup: &Backup,
) -> Result<LyricsOutcome> {
    let path = &track.path;
    // With --replace-lyrics we (re)write the tag even if one is already present.
    let need_lrc = !track.has_lrc;
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

    if cli.cache_only {
        return Ok(LyricsOutcome::CacheOnly {
            has_plain: record.has_plain(),
            has_synced: record.has_synced(),
            reused,
        });
    }

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
        )?
    } else {
        writer::Write::Skipped
    };
    let wrote_lrc = if need_lrc && record.has_synced() {
        writer::write_lrc(path, record.synced_lyrics.as_deref().unwrap())?
    } else {
        false
    };

    if tag.happened() || wrote_lrc {
        Ok(LyricsOutcome::Applied {
            tag,
            lrc: wrote_lrc,
            reused,
        })
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
    CacheOnly {
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

    if cli.cache_only {
        return Ok(CoverOutcome::CacheOnly { reused, source });
    }

    let embedded = if need_embed {
        writer::embed_cover(path, &art, replace, cli.verify, Some(backup))?
    } else {
        writer::Write::Skipped
    };
    let sidecar = match folder {
        Some(dir) if need_sidecar => writer::write_cover_sidecar(dir, &art, replace, Some(backup))?,
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
struct ArtistJob {
    mbid: String,
    name: String,
    folders: BTreeSet<PathBuf>,
    /// A track folder for this artist, named in the "no artist folder" log line.
    sample_dir: PathBuf,
}

/// Note the track's album artist for the artist-images pass. No network here.
fn collect_artist(
    root: &Path,
    track: &Track,
    meta: &TrackMetadata,
    jobs: &mut BTreeMap<String, ArtistJob>,
) {
    let Some(mbid) = meta.musicbrainz_album_artist_id.as_deref() else {
        return;
    };
    if mbid == musicbrainz::VARIOUS_ARTISTS {
        return;
    }
    let (Some(name), Some(dir)) = (meta.album_artist(), track.path.parent()) else {
        return;
    };
    let job = jobs.entry(mbid.to_string()).or_insert_with(|| ArtistJob {
        mbid: mbid.to_string(),
        name: name.to_string(),
        folders: BTreeSet::new(),
        sample_dir: dir.to_path_buf(),
    });
    if let Some(folder) = artist::find_artist_folder(dir, root, name) {
        job.folders.insert(folder);
    }
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
    CacheOnly {
        source: Source,
        reused: bool,
    },
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
    if job.folders.is_empty() && !cli.cache_only {
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
    if !look_again && !cli.cache_only && folders_complete(job, record.as_ref(), services.videos) {
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
    // that was never written (e.g. from --cache-only) says nothing about what's
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
    let needs_write =
        cli.cache_only || missing_files(job, &resolved, replace, alternatives, want_video);
    if !needs_write {
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

    if cli.cache_only {
        state.save_artist_record(mbid, &new_record)?;
        return Ok(ArtistOutcome::CacheOnly {
            source: resolved.source,
            reused,
        });
    }

    // 4. Write into every artist folder.
    let writes = (replace, alternatives);
    let written = write_artist_images(job, &images, video.as_ref(), writes, revisions)?;
    if written.wrote() || !written.alternatives.is_empty() {
        if written.wrote() {
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
                let saved = writer::archive_alternative(dir, *kind, art, revisions)?;
                written.alternatives.extend(saved);
                continue;
            }
            // Hand-curated in this folder: written if missing, never replaced.
            let replace = (replace && !kept) || skip::artist_image(dir, *kind);
            let (wrote, kept) = writer::write_artist_image(dir, *kind, art, replace, revisions)?;
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

fn log_lyrics(progress: &Progress, name: &str, outcome: &LyricsOutcome) {
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
                writer::Write::Created => parts.push("wrote lyrics tag".to_string()),
                writer::Write::Replaced => parts.push("replaced lyrics tag".to_string()),
                writer::Write::Skipped => {}
            }
            if *lrc {
                parts.push("wrote .lrc file".to_string());
            }
            let src = if *reused { " (from cache)" } else { "" };
            progress.info(format!("✓ {name}: {}{src}", parts.join(", ")));
        }
        LyricsOutcome::CacheOnly {
            has_plain,
            has_synced,
            reused,
        } => {
            let src = if *reused { "cache" } else { "api" };
            progress.info(format!(
                "~ {name}: lyrics via {src} (plain={has_plain}, synced={has_synced})"
            ));
        }
    }
}

fn log_cover(progress: &Progress, name: &str, outcome: &CoverOutcome) {
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
                writer::Write::Created => parts.push("embedded cover".to_string()),
                writer::Write::Replaced => parts.push("replaced embedded cover".to_string()),
                writer::Write::Skipped => {}
            }
            match sidecar {
                writer::Write::Created => parts.push("wrote cover.jpg".to_string()),
                writer::Write::Replaced => parts.push("replaced cover.jpg".to_string()),
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
        CoverOutcome::CacheOnly { reused, source } => {
            let src = if *reused { "cache" } else { source.as_str() };
            progress.info(format!("~ {name}: cover via {src}"));
        }
    }
}

fn log_artist(progress: &Progress, job: &ArtistJob, outcome: &ArtistOutcome) {
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
        ArtistOutcome::CacheOnly { source, reused } => {
            let src = if *reused { "cache" } else { source.label() };
            progress.info(format!("~ {name}: artist images via {src}"));
        }
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
                    writer::Write::Created => parts.push(format!("wrote {name}")),
                    writer::Write::Replaced => parts.push(format!("replaced {name}")),
                    writer::Write::Skipped => {}
                }
            }
            if !alternatives.is_empty() {
                parts.push(format!(
                    "archived alternative {} (gatefold restore)",
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
    lyrics_skipped: u64,
    lyrics_not_found: u64,
    no_lyrics: u64,
    lyrics_cache_only: u64,
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
    cover_cache_only: u64,
    cover_errors: u64,
    // artist images
    artist_wrote: u64,
    artist_replaced: u64,
    artist_unchanged: u64,
    artist_skipped: u64,
    artist_kept: u64,
    artist_fresh: u64,
    artist_no_folder: u64,
    artist_spotify_only: u64,
    artist_no_links: u64,
    artist_no_image: u64,
    artist_cache_only: u64,
    artist_errors: u64,
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
            LyricsOutcome::CacheOnly { .. } => self.lyrics_cache_only += 1,
            LyricsOutcome::Applied { tag, lrc, .. } => {
                match tag {
                    writer::Write::Created => self.wrote_tag += 1,
                    writer::Write::Replaced => self.replaced_tag += 1,
                    writer::Write::Skipped => {}
                }
                if *lrc {
                    self.wrote_lrc += 1;
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
            CoverOutcome::CacheOnly { .. } => self.cover_cache_only += 1,
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
            ArtistOutcome::CacheOnly { .. } => self.artist_cache_only += 1,
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
        let cache_only = cli.cache_only;
        if self.unreadable > 0 {
            progress.info(format!(
                "unreadable: {} file(s) could not be parsed and were skipped",
                self.unreadable
            ));
        }
        if cli.lyrics_enabled() {
            self.print_lyrics(progress, cache_only);
        }
        if cli.cover_enabled() {
            self.print_cover(progress, cache_only);
        }
        if cli.artist_images_enabled() {
            self.print_artist(progress, cache_only);
        }
    }

    fn print_artist(&self, progress: &Progress, cache_only: bool) {
        progress.info("artist images:");
        if cache_only {
            progress.info(format!("  cached {} artist(s)", self.artist_cache_only));
        } else {
            progress.info(format!(
                "  wrote images for {} artist(s), replaced {}",
                self.artist_wrote, self.artist_replaced
            ));
        }
        progress.info(format!(
            "  unchanged {}, skipped {}, kept {}, fresh {}, no folder {}, no apple music link {}, no links {}, no image {}, errors {}",
            self.artist_unchanged,
            self.artist_skipped,
            self.artist_kept,
            self.artist_fresh,
            self.artist_no_folder,
            self.artist_spotify_only,
            self.artist_no_links,
            self.artist_no_image,
            self.artist_errors
        ));
    }

    fn print_lyrics(&self, progress: &Progress, cache_only: bool) {
        progress.info("lyrics:");
        if cache_only {
            progress.info(format!("  cached {} response(s)", self.lyrics_cache_only));
        } else {
            progress.info(format!(
                "  wrote {} tag(s), replaced {}",
                self.wrote_tag, self.replaced_tag
            ));
            progress.info(format!("  created {} .lrc file(s)", self.wrote_lrc));
        }
        progress.info(format!(
            "  skipped {}, not found {}, none {}, errors {}",
            self.lyrics_skipped, self.lyrics_not_found, self.no_lyrics, self.lyrics_errors
        ));
    }

    fn print_cover(&self, progress: &Progress, cache_only: bool) {
        progress.info("cover art:");
        if cache_only {
            progress.info(format!("  cached {} image(s)", self.cover_cache_only));
        } else {
            progress.info(format!(
                "  embedded {} cover(s), replaced {}",
                self.embedded_cover, self.replaced_cover
            ));
            progress.info(format!(
                "  wrote {} cover.jpg sidecar(s), replaced {}",
                self.wrote_cover_sidecar, self.replaced_cover_sidecar
            ));
        }
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
        )
        .unwrap();
        assert!(!skipped.join("folder.jpg").exists());
        assert_eq!(archived(&skipped), vec![b"rejected".to_vec()]);
        assert_eq!(written.alternatives.len(), 1);

        fs::remove_dir_all(&root).unwrap();
    }
}
