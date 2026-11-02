mod apple;
mod artist;
mod backup;
mod cli;
mod coverart;
mod lrclib;
mod metadata;
mod musicbrainz;
mod net;
mod progress;
mod revision;
mod scanner;
mod state;
mod verify;
mod video;
mod writer;

use anyhow::{Context, Result};
use artist::{ArtistRecord, Resolved, Source};
use backup::Backup;
use clap::Parser;
use cli::Cli;
use lrclib::{Client, Fetched, LyricsRecord, Query, Unauthorized};
use metadata::TrackMetadata;
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

    if cli.verify {
        verify::ensure_available()?;
    }

    let state = State::new(&cli.state_dir)
        .with_context(|| format!("preparing state dir {}", cli.state_dir.display()))?;

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
    let root = std::fs::canonicalize(&cli.path).unwrap_or_else(|_| cli.path.clone());
    let tracks = scanner::scan(&root);
    let progress = Progress::new(tracks.len(), seed_secs);

    let mb_client = musicbrainz::Client::new(&cli.musicbrainz_url, cli.user_agent(), delay);
    let mb_host = (!mb_client.is_public()).then(|| mb_client.host());
    progress.headline(&run_summary(&cli, mb_host.as_deref()));

    progress.section("Scanning");
    progress.info(format!(
        "found {} music file(s) under {}",
        tracks.len(),
        root.display()
    ));
    let with_lrc = tracks.iter().filter(|t| t.has_lrc).count();
    progress.info(format!(
        "{with_lrc} already have an adjacent .lrc, {} do not",
        tracks.len() - with_lrc
    ));

    progress.section("Estimating");
    progress.log_initial_estimate();

    let videos = cli.artist_images_enabled() && !cli.skip_artist_videos;
    let ffmpeg = videos && video::ffmpeg_available();
    if videos && !ffmpeg {
        progress.info(
            "ffmpeg not found on PATH: motion backdrops (backdrops/backdrop.mp4) will be skipped",
        );
    }
    let services = Services {
        lyrics: Client::new(
            cli.user_agent(),
            cli.app_id.clone(),
            cli.api_key.clone(),
            delay,
        ),
        cover: coverart::Client::new(cli.user_agent(), delay),
        musicbrainz: mb_client,
        lookup: apple::Lookup::new(cli.user_agent(), delay),
        resolver: artist::Resolver::new(cli.user_agent(), delay),
        videos: ffmpeg,
    };
    let revisions = Revisions::new();

    progress.section(if cli.cache_only {
        "Processing (cache-only)"
    } else {
        "Processing"
    });

    let mut summary = Summary::default();
    let artist_jobs = run_track_pass(
        &cli,
        &root,
        &state,
        &services,
        &progress,
        &tracks,
        &mut summary,
    )?;
    progress.finish();

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

/// Every remote service a run talks to, built once.
struct Services {
    lyrics: Client,
    cover: coverart::Client,
    musicbrainz: musicbrainz::Client,
    lookup: apple::Lookup,
    resolver: artist::Resolver,
    /// Motion backdrops are wanted and ffmpeg is available to remux them.
    videos: bool,
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
    let mut cover_misses: HashSet<String> = HashSet::new();
    let mut artist_jobs: BTreeMap<String, ArtistJob> = BTreeMap::new();
    for (i, track) in tracks.iter().enumerate() {
        let name = track.path.file_name().map_or_else(
            || track.path.to_string_lossy().into_owned(),
            |n| pad_track_number(&n.to_string_lossy()),
        );
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
    let replace = join_enabled(&[
        ("lyrics", cli.replace_lyrics),
        ("cover", cli.replace_cover),
        ("artist images", cli.replace_artist_images),
    ]);
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
    cover_misses: &mut HashSet<String>,
    artist_jobs: &mut BTreeMap<String, ArtistJob>,
) -> Result<()> {
    let abs = std::fs::canonicalize(&track.path).unwrap_or_else(|_| track.path.clone());
    let digest = digest_for(&abs);

    // Metadata cache: reuse a current-schema entry, otherwise (missing or from
    // an older build) parse tags and persist. A file lofty can't read is logged
    // and skipped rather than aborting the whole run.
    let meta: TrackMetadata = match state.load_metadata::<TrackMetadata>(&digest)? {
        Some(meta) if meta.is_current() => meta,
        _ => match metadata::extract(&track.path) {
            Ok(meta) => {
                state.save_metadata(&digest, &meta)?;
                meta
            }
            Err(err) => {
                summary.unreadable += 1;
                progress.info(format!("✗ {name}: {err:#}"));
                return Ok(());
            }
        },
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
    Ok(())
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

    // Reuse a cached hit; otherwise (re)query. A recorded miss is deliberately
    // ignored here — only --new-only consults it — so a normal run rechecks it.
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

/// What happened to a track's cover art.
enum CoverOutcome {
    SkippedNoMbid,
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
    state: &State,
    services: &Services,
    progress: &Progress,
    track: &Track,
    meta: &TrackMetadata,
    cover_misses: &mut HashSet<String>,
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
    // With --replace-cover we (re)write even when a cover is already present.
    let need_embed = !meta.has_cover_art || cli.replace_cover;
    let need_sidecar =
        folder.is_some_and(|dir| !writer::folder_has_cover(dir) || cli.replace_cover);
    if !need_embed && !need_sidecar {
        return Ok(CoverOutcome::SkippedNothingToDo);
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

    // Reuse a cached hit; else a within-run miss (so a release without art isn't
    // re-queried for every track); else look it up. The persisted `.miss` marker is
    // deliberately ignored outside --new-only, so a normal run rechecks it.
    let (art, reused) = if state.has_art(&key) {
        (state.load_art(&key)?, true)
    } else if cover_misses.contains(&key) {
        (None, true)
    } else {
        let fetched = fetch_cover(services, state, progress, &key, meta, release, group)?;
        if fetched.is_none() {
            cover_misses.insert(key.clone());
        }
        (fetched, false)
    };

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
        writer::embed_cover(
            path,
            &art.mime,
            &art.data,
            cli.replace_cover,
            cli.verify,
            Some(backup),
        )?
    } else {
        writer::Write::Skipped
    };
    let sidecar = match folder {
        Some(dir) if need_sidecar => {
            writer::write_cover_sidecar(dir, &art.ext, &art.data, cli.replace_cover, Some(backup))?
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
/// Caches and returns a success, `None` when nothing was found.
#[allow(clippy::too_many_arguments)]
fn fetch_cover(
    services: &Services,
    state: &State,
    progress: &Progress,
    key: &str,
    meta: &TrackMetadata,
    release: &str,
    group: Option<&str>,
) -> Result<Option<state::CachedArt>> {
    let recheck = state.has_cover_miss(key);
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
        state.save_art(key, &image.ext, &image.mime, Some(source), &image.data)?;
        Ok(Some(state::CachedArt {
            mime: image.mime,
            ext: image.ext,
            data: image.data,
            source: Some(source.to_string()),
        }))
    } else {
        state.save_cover_miss(key)?;
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
    SkippedNothingToDo,
    SkippedByFilter,
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
        source: Source,
        reused: bool,
        /// Where replaced files were moved, relative to the artist folder.
        kept: Vec<String>,
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
    let (mb, resolver) = (&services.musicbrainz, &services.resolver);
    let mbid = job.mbid.as_str();
    let replacing = cli.replace_artist_images;
    if job.folders.is_empty() && !cli.cache_only {
        return Ok(ArtistOutcome::NoFolder);
    }

    let record: Option<ArtistRecord> = state
        .load_artist_record::<ArtistRecord>(mbid)?
        .filter(ArtistRecord::is_current);

    // Every folder already complete and not replacing: nothing to do, and no
    // reason to touch the network.
    if !replacing && !cli.cache_only && folders_complete(job, record.as_ref(), services.videos) {
        return Ok(ArtistOutcome::SkippedNothingToDo);
    }

    // --new-only: only artists never looked up. --miss-only: only recorded misses.
    if cli.new_only && state.has_artist_state(mbid) {
        return Ok(ArtistOutcome::SkippedByFilter);
    }
    if cli.miss_only && !state.has_artist_miss(mbid) {
        return Ok(ArtistOutcome::SkippedByFilter);
    }
    // A recorded miss is rechecked from scratch (links included), like cover art.
    let rechecking = state.has_artist_miss(mbid);

    // 1. Links, via MusicBrainz (cached per artist).
    let Some(links) = artist_links(state, mb, progress, mbid, rechecking)? else {
        state.save_artist_miss(mbid)?;
        return Ok(ArtistOutcome::NoLinks);
    };

    // 2. Image URLs, from the artist's page. A replace run always re-resolves so
    //    a changed photo (which changes the URL) is noticed.
    let reuse = record.as_ref().filter(|_| !replacing && !rechecking);
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
    let replace = replacing && record.as_ref().is_none_or(|r| !r.same_images(&resolved));
    let now = revisions.stamp();
    let mut new_record = record
        .clone()
        .filter(|r| r.same_images(&resolved))
        .unwrap_or_else(|| ArtistRecord::new(mbid, &job.name, &resolved, now));
    let nothing_to_do = if replacing {
        ArtistOutcome::Unchanged
    } else {
        ArtistOutcome::SkippedNothingToDo
    };

    let want_video = services.videos && resolved.video_url.is_some();
    let needs_write = cli.cache_only || missing_files(job, &resolved, replace, want_video);
    if !needs_write {
        if record.is_none() {
            state.save_artist_record(mbid, &new_record)?;
        }
        return Ok(nothing_to_do);
    }

    // 3. Bytes (cached by image URL, so a changed photo is a fresh download).
    let Some((avatar, backdrop)) = artist_images(state, resolver, progress, job, &resolved)? else {
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
    let written = write_artist_images(
        job,
        &avatar,
        backdrop.as_ref(),
        video.as_ref(),
        replace,
        revisions,
    )?;
    if written.avatar.happened() || written.backdrop.happened() || written.video.happened() {
        new_record.written_at = Some(now);
        state.save_artist_record(mbid, &new_record)?;
        Ok(ArtistOutcome::Applied {
            avatar: written.avatar,
            backdrop: written.backdrop,
            video: written.video,
            source: resolved.source,
            reused,
            kept: written.kept,
        })
    } else {
        if record.is_none() {
            state.save_artist_record(mbid, &new_record)?;
        }
        Ok(nothing_to_do)
    }
}

/// True when some artist folder lacks a file the resolution can provide (or a
/// replace is due, which rewrites regardless).
fn missing_files(job: &ArtistJob, resolved: &Resolved, replace: bool, want_video: bool) -> bool {
    job.folders.iter().any(|dir| {
        replace
            || ArtistImage::Avatar.existing(dir).is_none()
            || (resolved.backdrop_url.is_some() && ArtistImage::Backdrop.existing(dir).is_none())
            || (want_video && ArtistImage::Video.existing(dir).is_none())
    })
}

/// True when every artist folder already has the files the source can provide
/// (a Spotify-sourced artist has no backdrop to wait for; a video only counts
/// when the artist is known to have one and ffmpeg is available).
fn folders_complete(job: &ArtistJob, record: Option<&ArtistRecord>, videos: bool) -> bool {
    let has_backdrop_source = record.is_none_or(|r| r.backdrop_url.is_some());
    let has_video_source = videos && record.is_none_or(|r| r.video_url.is_some());
    job.folders.iter().all(|dir| {
        ArtistImage::Avatar.existing(dir).is_some()
            && (!has_backdrop_source || ArtistImage::Backdrop.existing(dir).is_some())
            && (!has_video_source || ArtistImage::Video.existing(dir).is_some())
    })
}

/// The artist's streaming links from its `MusicBrainz` lookup (cached per artist).
/// `None` when the artist is unknown or has no Apple Music / Spotify link.
fn artist_links(
    state: &State,
    mb: &musicbrainz::Client,
    progress: &Progress,
    mbid: &str,
    rechecking: bool,
) -> Result<Option<musicbrainz::ArtistLinks>> {
    let cached = if rechecking {
        None
    } else {
        state.load_musicbrainz_artist(mbid)?
    };
    let lookup = if let Some(value) = cached {
        value
    } else {
        let Some(value) = mb.artist(mbid, progress)? else {
            return Ok(None);
        };
        state.save_musicbrainz_artist(mbid, &value)?;
        value
    };
    let links = musicbrainz::links_from(&lookup);
    Ok((!links.is_empty()).then_some(links))
}

/// What writing an artist's files across its folders amounted to.
struct Written {
    avatar: writer::Write,
    backdrop: writer::Write,
    video: writer::Write,
    /// Where replaced files were moved, relative to the artist folder.
    kept: Vec<String>,
}

/// Write the avatar (and backdrop and video, when there are any) into every
/// folder of the job. A replace moves the previous file into its archive folder
/// (`primary/`, `backdrops/`) first.
fn write_artist_images(
    job: &ArtistJob,
    avatar: &state::CachedArt,
    backdrop: Option<&state::CachedArt>,
    video: Option<&state::CachedArt>,
    replace: bool,
    revisions: &Revisions,
) -> Result<Written> {
    let mut written = Written {
        avatar: writer::Write::Skipped,
        backdrop: writer::Write::Skipped,
        video: writer::Write::Skipped,
        kept: Vec::new(),
    };
    let files = [
        (ArtistImage::Avatar, Some(avatar)),
        (ArtistImage::Backdrop, backdrop),
        (ArtistImage::Video, video),
    ];
    for dir in &job.folders {
        for (kind, art) in &files {
            let Some(art) = art else { continue };
            let (wrote, kept) =
                writer::write_artist_image(dir, *kind, &art.ext, &art.data, replace, revisions)?;
            let slot = match kind {
                ArtistImage::Avatar => &mut written.avatar,
                ArtistImage::Backdrop => &mut written.backdrop,
                ArtistImage::Video => &mut written.video,
            };
            *slot = strongest(*slot, wrote);
            written.kept.extend(kept);
        }
    }
    Ok(written)
}

/// The avatar and (when the source has one) backdrop bytes for a resolution.
/// `None` when the avatar is unavailable; a missing backdrop is logged and skipped.
fn artist_images(
    state: &State,
    resolver: &artist::Resolver,
    progress: &Progress,
    job: &ArtistJob,
    urls: &Resolved,
) -> Result<Option<(state::CachedArt, Option<state::CachedArt>)>> {
    let Some(avatar) = artist_image(state, resolver, progress, &urls.avatar_url)? else {
        return Ok(None);
    };
    let backdrop = match &urls.backdrop_url {
        Some(url) => {
            let image = artist_image(state, resolver, progress, url)?;
            if image.is_none() {
                progress.info(format!(
                    "· {}: backdrop unavailable, keeping avatar only",
                    job.name
                ));
            }
            image
        }
        None => None,
    };
    Ok(Some((avatar, backdrop)))
}

/// An artist's motion backdrop: from the art cache, else the best fitting HLS
/// variant remuxed to MP4 by ffmpeg and cached. `None` (logged) when the
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
        let result = video::remux(&variant.url, resolver.user_agent(), &work)
            .and_then(|()| std::fs::read(&work).context("reading remuxed video"));
        let _ = std::fs::remove_file(&work);
        result.map(Some)
    })();
    match remuxed {
        Ok(Some(data)) => {
            state.save_art(&key, "mp4", "video/mp4", Some("apple music"), &data)?;
            Ok(Some(state::CachedArt {
                mime: "video/mp4".to_string(),
                ext: "mp4".to_string(),
                data,
                source: Some("apple music".to_string()),
            }))
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
            state.save_art(
                &key,
                &image.ext,
                &image.mime,
                Some("apple music"),
                &image.data,
            )?;
            Ok(Some(state::CachedArt {
                mime: image.mime,
                ext: image.ext,
                data: image.data,
                source: Some("apple music".to_string()),
            }))
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
        CoverOutcome::SkippedNoMbid
        | CoverOutcome::SkippedNothingToDo
        | CoverOutcome::SkippedByFilter => {}
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
        ArtistOutcome::NoLinks => {
            progress.info(format!(
                "· {name}: no Apple Music/Spotify link on MusicBrainz"
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
            source,
            reused,
            kept,
        } => {
            let mut parts = Vec::new();
            for (kind, ext, write) in [
                (ArtistImage::Avatar, "jpg", avatar),
                (ArtistImage::Backdrop, "jpg", backdrop),
                (ArtistImage::Video, "mp4", video),
            ] {
                let name = kind.label(ext);
                match write {
                    writer::Write::Created => parts.push(format!("wrote {name}")),
                    writer::Write::Replaced => parts.push(format!("replaced {name}")),
                    writer::Write::Skipped => {}
                }
            }
            let src = if *reused { "cache" } else { source.label() };
            let kept = if kept.is_empty() {
                String::new()
            } else {
                format!(", previous in {}", kept.join(", "))
            };
            progress.info(format!("✓ {name}: {} ({src}{kept})", parts.join(", ")));
        }
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
    cover_not_found: u64,
    cover_cache_only: u64,
    cover_errors: u64,
    // artist images
    artist_wrote: u64,
    artist_replaced: u64,
    artist_unchanged: u64,
    artist_skipped: u64,
    artist_no_folder: u64,
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
            ArtistOutcome::NoLinks => self.artist_no_links += 1,
            ArtistOutcome::NoImage => self.artist_no_image += 1,
            ArtistOutcome::Unchanged => self.artist_unchanged += 1,
            ArtistOutcome::CacheOnly { .. } => self.artist_cache_only += 1,
            ArtistOutcome::Applied {
                avatar,
                backdrop,
                video,
                ..
            } => {
                if [avatar, backdrop, video].contains(&&writer::Write::Replaced) {
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
            "  unchanged {}, skipped {}, no folder {}, no links {}, no image {}, errors {}",
            self.artist_unchanged,
            self.artist_skipped,
            self.artist_no_folder,
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
            "  skipped {}, not found {}, errors {}",
            self.cover_skipped, self.cover_not_found, self.cover_errors
        ));
    }
}
