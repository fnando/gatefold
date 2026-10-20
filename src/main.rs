mod backup;
mod cli;
mod coverart;
mod lrclib;
mod metadata;
mod net;
mod progress;
mod scanner;
mod state;
mod verify;
mod writer;

use anyhow::{Context, Result};
use backup::Backup;
use clap::Parser;
use cli::Cli;
use lrclib::{Client, Fetched, LyricsRecord, Query, Unauthorized};
use metadata::TrackMetadata;
use progress::Progress;
use scanner::Track;
use state::{State, digest_for, digest_str};
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    load_env_file()?;
    let cli = Cli::parse();

    if cli.verify {
        verify::ensure_available()?;
    }

    let state = State::new(&cli.state_dir)
        .with_context(|| format!("preparing state dir {}", cli.state_dir.display()))?;

    let delay = Duration::from_millis(cli.delay_ms);
    // Seed per-track estimate: up to two requests (lyrics + cover) plus rough RTT.
    let seed_secs = delay.as_secs_f64().mul_add(2.0, 1.0);

    let tracks = scanner::scan(&cli.path);
    let progress = Progress::new(tracks.len(), seed_secs);

    progress.headline(&run_summary(&cli));

    progress.section("Scanning");
    progress.info(format!(
        "found {} music file(s) under {}",
        tracks.len(),
        cli.path.display()
    ));
    let with_lrc = tracks.iter().filter(|t| t.has_lrc).count();
    progress.info(format!(
        "{with_lrc} already have an adjacent .lrc, {} do not",
        tracks.len() - with_lrc
    ));

    progress.section("Estimating");
    progress.log_initial_estimate();

    let lyrics_client = Client::new(
        cli.user_agent(),
        cli.app_id.clone(),
        cli.api_key.clone(),
        delay,
    );
    let cover_client = coverart::Client::new(cli.user_agent(), delay);

    progress.section(if cli.cache_only {
        "Processing (cache-only)"
    } else {
        "Processing"
    });

    let mut summary = Summary::default();
    let mut cover_misses: HashSet<String> = HashSet::new();
    for (i, track) in tracks.iter().enumerate() {
        let name = track.path.file_name().map_or_else(
            || track.path.to_string_lossy().into_owned(),
            |n| pad_track_number(&n.to_string_lossy()),
        );
        progress.status(name.clone());

        let start = Instant::now();
        let result = process_track(
            &cli,
            &state,
            &lyrics_client,
            &cover_client,
            &progress,
            track,
            &name,
            &mut summary,
            &mut cover_misses,
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

    progress.finish();
    progress.section("Summary");
    summary.print(&progress, cli.cache_only);
    Ok(())
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
fn run_summary(cli: &Cli) -> String {
    let mode = if cli.cache_only {
        "cache-only"
    } else {
        "writing"
    };
    let scope = if cli.lyrics_only {
        "lyrics"
    } else if cli.cover_only {
        "cover"
    } else {
        "lyrics + cover"
    };
    let select = if cli.new_only {
        "new-only"
    } else if cli.miss_only {
        "missed-only"
    } else {
        "all"
    };
    let replace = match (cli.replace_lyrics, cli.replace_cover) {
        (true, true) => "lyrics + cover",
        (true, false) => "lyrics",
        (false, true) => "cover",
        (false, false) => "none",
    };
    let verify = if cli.verify { "on" } else { "off" };
    format!(
        "gatefold · {mode} · scope: {scope} · select: {select} · replace: {replace} · verify: {verify}"
    )
}

/// Resolve metadata (cached or freshly parsed) then run the lyrics and cover-art
/// sub-flows. Only a fatal lyrics error (`401`) propagates; per-track failures
/// are logged and counted.
#[allow(clippy::too_many_arguments)]
fn process_track(
    cli: &Cli,
    state: &State,
    lyrics_client: &Client,
    cover_client: &coverart::Client,
    progress: &Progress,
    track: &Track,
    name: &str,
    summary: &mut Summary,
    cover_misses: &mut HashSet<String>,
) -> Result<()> {
    let abs = std::fs::canonicalize(&track.path).unwrap_or_else(|_| track.path.clone());
    let digest = digest_for(&abs);

    // Metadata cache: reuse a current-schema entry, otherwise (missing or from
    // an older build) parse tags and persist.
    let meta: TrackMetadata = match state.load_metadata::<TrackMetadata>(&digest)? {
        Some(meta) if meta.is_current() => meta,
        _ => {
            let meta = metadata::extract(&track.path)?;
            state.save_metadata(&digest, &meta)?;
            meta
        }
    };

    // Previous tag values are snapshotted here before `--replace` overwrites them.
    let backup = Backup::new(state.backup_root(), &digest, &track.path);

    if !cli.cover_only {
        match process_lyrics(
            cli,
            state,
            lyrics_client,
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

    if !cli.lyrics_only {
        match process_cover(
            cli,
            state,
            cover_client,
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
        sidecar: bool,
        reused: bool,
    },
    CacheOnly {
        reused: bool,
    },
}

#[allow(clippy::too_many_arguments)]
fn process_cover(
    cli: &Cli,
    state: &State,
    client: &coverart::Client,
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
    // With --replace-cover we (re)embed even when a front cover is already present.
    let need_embed = !meta.has_cover_art || cli.replace_cover;
    let need_sidecar = folder.is_some_and(|dir| !writer::folder_has_cover(dir));
    if !need_embed && !need_sidecar {
        return Ok(CoverOutcome::SkippedNothingToDo);
    }

    // The art cache is keyed by release id (plus the image variant, so a variant
    // change re-fetches), shared across every track on the release — only the
    // first track of an album hits the network.
    let key = digest_str(&format!("{release}:{}", coverart::FRONT_VARIANT));

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
        let fetched = fetch_cover(client, state, progress, &key, release, group)?;
        if fetched.is_none() {
            cover_misses.insert(key.clone());
        }
        (fetched, false)
    };

    let Some(art) = art else {
        return Ok(CoverOutcome::NotFound);
    };

    if cli.cache_only {
        return Ok(CoverOutcome::CacheOnly { reused });
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
    let wrote_sidecar = match folder {
        Some(dir) if need_sidecar => writer::write_cover_sidecar(dir, &art.ext, &art.data)?,
        _ => false,
    };

    if embedded.happened() || wrote_sidecar {
        Ok(CoverOutcome::Applied {
            embedded,
            sidecar: wrote_sidecar,
            reused,
        })
    } else {
        // Art was available but the file already had it — nothing to do.
        Ok(CoverOutcome::SkippedNothingToDo)
    }
}

/// Download the front cover for a release, falling back to its release group
/// (art is often attached there). Caches and returns a success, `None` for 404.
fn fetch_cover(
    client: &coverart::Client,
    state: &State,
    progress: &Progress,
    key: &str,
    release: &str,
    group: Option<&str>,
) -> Result<Option<state::CachedArt>> {
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

    if let Some(image) = image {
        state.save_art(key, &image.ext, &image.mime, &image.data)?;
        Ok(Some(state::CachedArt {
            mime: image.mime,
            ext: image.ext,
            data: image.data,
        }))
    } else {
        state.save_cover_miss(key)?;
        Ok(None)
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
        } => {
            let mut parts = Vec::new();
            match embedded {
                writer::Write::Created => parts.push("embedded cover".to_string()),
                writer::Write::Replaced => parts.push("replaced embedded cover".to_string()),
                writer::Write::Skipped => {}
            }
            if *sidecar {
                parts.push("wrote cover.jpg".to_string());
            }
            if parts.is_empty() {
                return;
            }
            let src = if *reused { " (from cache)" } else { "" };
            progress.info(format!("✓ {name}: {}{src}", parts.join(", ")));
        }
        CoverOutcome::CacheOnly { reused } => {
            let src = if *reused { "cache" } else { "archive" };
            progress.info(format!("~ {name}: cover via {src}"));
        }
    }
}

/// Aggregate counts printed at the end of the run.
#[derive(Default)]
struct Summary {
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
    cover_skipped: u64,
    cover_not_found: u64,
    cover_cache_only: u64,
    cover_errors: u64,
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
                if *sidecar {
                    self.wrote_cover_sidecar += 1;
                }
            }
        }
    }

    fn print(&self, progress: &Progress, cache_only: bool) {
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

        progress.info("cover art:");
        if cache_only {
            progress.info(format!("  cached {} image(s)", self.cover_cache_only));
        } else {
            progress.info(format!(
                "  embedded {} cover(s), replaced {}",
                self.embedded_cover, self.replaced_cover
            ));
            progress.info(format!(
                "  wrote {} cover.jpg sidecar(s)",
                self.wrote_cover_sidecar
            ));
        }
        progress.info(format!(
            "  skipped {}, not found {}, errors {}",
            self.cover_skipped, self.cover_not_found, self.cover_errors
        ));
    }
}
