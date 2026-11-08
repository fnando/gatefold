use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

/// Scan a folder of music files and backfill lyrics, cover art, and artist images.
#[derive(Debug, Parser)]
#[command(
    name = "gatefold",
    version,
    about,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
#[allow(clippy::struct_excessive_bools)] // CLI flags are naturally independent bools
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Folder to scan recursively for music files.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Base directory for cached API responses (`lrc/`, `artist/`, `release/`),
    /// downloaded images (`art/`), and metadata (`metadata/`).
    #[arg(long, value_name = "DIR", required = true)]
    pub state_dir: Option<PathBuf>,

    /// Secret `Lrclib-Api-Key` header (server-side model). Pair with --app-id.
    /// Prefer --env-file on shared hosts: a flag value is visible in `ps`.
    #[arg(
        long,
        value_name = "KEY",
        env = "GATEFOLD_LRCLIB_API_KEY",
        hide_env_values = true
    )]
    pub api_key: Option<String>,

    /// `Lrclib-App-Id` header. Required alongside --api-key to unlock the higher tier.
    #[arg(long, value_name = "ID", env = "GATEFOLD_LRCLIB_APP_ID")]
    pub app_id: Option<String>,

    /// Load `GATEFOLD_LRCLIB_API_KEY` / `GATEFOLD_LRCLIB_APP_ID` / `GATEFOLD_USER_AGENT` /
    /// `GATEFOLD_MUSICBRAINZ_URL` from a `KEY=VALUE` env file, keeping secrets out of
    /// the process args (and `ps`). Flags and real environment variables override the
    /// file. Exits with an error if the file can't be read.
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    /// Override the `User-Agent` sent to lrclib.net, the Cover Art Archive, Apple, and
    /// `MusicBrainz` (which wants a contact URL or email in it).
    #[arg(long, value_name = "UA", env = "GATEFOLD_USER_AGENT")]
    pub user_agent: Option<String>,

    /// Base URL of the `MusicBrainz` web service used to find an artist's streaming
    /// pages. Point it at a local mirror to avoid the public server's 1 request/second
    /// limit, which is enforced only for musicbrainz.org.
    #[arg(
        long,
        value_name = "URL",
        env = "GATEFOLD_MUSICBRAINZ_URL",
        default_value = "https://musicbrainz.org"
    )]
    pub musicbrainz_url: String,

    /// Delay between API requests in milliseconds (docs recommend 200-500).
    #[arg(long, value_name = "MS", default_value_t = 300)]
    pub delay_ms: u64,

    /// Populate the caches (lookups + downloads) but make no changes to music files
    /// or folders.
    #[arg(long)]
    pub cache_only: bool,

    /// Replace an existing embedded lyrics tag instead of keeping it. The previous
    /// value is snapshotted under the state dir's `backup/` tree so it can be restored.
    #[arg(long)]
    pub replace_lyrics: bool,

    /// Replace an existing embedded cover and `cover.jpg` sidecar instead of keeping
    /// them (e.g. to upgrade to Apple Music's larger artwork). The previous values
    /// are snapshotted under the state dir's `backup/` tree so they can be restored.
    #[arg(long)]
    pub replace_cover: bool,

    /// Replace an artist folder's `folder.jpg` / `backdrop.jpg` / motion backdrop /
    /// `logo.png` when the artist's image has changed since it was written. The
    /// previous files are kept, dated, in the artist folder's hidden
    /// `.gatefold-archive/`.
    #[arg(long, conflicts_with_all = ["skip_artist_images", "lyrics_only", "cover_only"])]
    pub replace_artist_images: bool,

    /// On a --replace-artist-images run, don't re-check an artist's page if it was
    /// last checked within this long ago (e.g. `30d`, `12h`; units: s, m, h, d, w).
    /// Keeps a scheduled replace run from hitting Apple Music/Spotify for every
    /// artist every time. Missing images are still written.
    #[arg(long, value_name = "DURATION", requires = "replace_artist_images", value_parser = parse_duration)]
    pub artist_refresh_after: Option<Duration>,

    /// Validate each tag write with ffprobe before it replaces the original: changes
    /// are applied to a temp copy, verified, then atomically swapped in. Requires
    /// ffprobe (ffmpeg) on PATH.
    #[arg(long)]
    pub verify: bool,

    /// Only look up items with no result on record yet (no cached hit or recorded
    /// "not found"). Use for fast incremental runs after a full pass.
    #[arg(long)]
    pub new_only: bool,

    /// Only re-check items previously recorded as "not found", skipping brand-new and
    /// already-resolved items. Useful to retry misses after lrclib/CAA add content.
    #[arg(long, conflicts_with = "new_only")]
    pub miss_only: bool,

    /// Don't retry an item recorded as "not found" within this long ago (e.g. `7d`,
    /// `12h`; units: s, m, h, d, w). Applies wherever a miss would be rechecked: normal
    /// runs, --miss-only, and --replace-artist-images.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub retry_missed_after: Option<Duration>,

    /// Only process lyrics; skip cover art and artist images entirely.
    #[arg(long, conflicts_with = "cover_only")]
    pub lyrics_only: bool,

    /// Only process cover art; skip lyrics and artist images entirely.
    #[arg(long)]
    pub cover_only: bool,

    /// Only process artist images; skip lyrics and cover art entirely.
    #[arg(long, conflicts_with_all = ["lyrics_only", "cover_only"])]
    pub artist_images_only: bool,

    /// Skip the artist-images pass (lyrics and cover art only).
    #[arg(long, conflicts_with = "artist_images_only")]
    pub skip_artist_images: bool,

    /// Don't download Apple Music motion backdrops (`backdrops/backdrop.mp4`, remuxed
    /// with ffmpeg); still images only.
    #[arg(long)]
    pub skip_artist_videos: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Reject an image you don't want, but stay open to a different one: writes a
    /// `.gatefold-<sha256>-skip` marker next to it. Later runs replace the image as
    /// soon as another one is available and never write these exact contents
    /// again. Delete the marker to undo.
    Skip {
        /// Album covers (`cover.jpg`) or artist files (`folder.jpg`,
        /// `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`).
        #[arg(required = true, value_name = "IMAGE")]
        images: Vec<PathBuf>,
    },
    /// Never replace an image, even with `--replace-cover` /
    /// `--replace-artist-images`: writes the keep marker for its kind
    /// (`.gatefold-primary-keep`, `.gatefold-cover-keep`, …). A folder gets
    /// `.gatefold-keep`, keeping everything it governs. A skip of the image is
    /// taken back. Delete the marker to undo.
    Keep {
        /// Album covers (`cover.jpg`), artist files (`folder.jpg`,
        /// `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`), or folders.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Bring back an artist file from an artist folder's `.gatefold-archive/`.
    /// The file it replaces is skipped (see `skip`) and archived.
    Restore {
        /// Artist folder: list what's in its `.gatefold-archive/` and ask which
        /// one to restore.
        #[arg(
            long,
            value_name = "DIR",
            required_unless_present = "file",
            conflicts_with = "file"
        )]
        dir: Option<PathBuf>,

        /// A file in an artist folder's `.gatefold-archive/`, restored without
        /// asking.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,

        /// Also keep the restored file (see `keep`), so `--replace-artist-images`
        /// never replaces it.
        #[arg(long)]
        keep: bool,
    },
}

/// `30d`, `12h`, `90m`, `45s`, `2w`: a whole number plus a unit.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &text[digits.len()..];
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("expected a number followed by s/m/h/d/w, got `{text}`"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => {
            return Err(format!(
                "unknown unit `{unit}` in `{text}`; use s, m, h, d or w"
            ));
        }
    };
    n.checked_mul(secs)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}` is too long"))
}

/// `30d` / `12h` / `90m` / `45s`, using the largest unit that divides evenly.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    for (unit, size) in [("w", 7 * 86_400), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs > 0 && secs.is_multiple_of(size) {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

impl Cli {
    /// Resolve the `User-Agent`, falling back to a descriptive default.
    pub fn user_agent(&self) -> String {
        self.user_agent.clone().unwrap_or_else(|| {
            format!(
                "gatefold/{} (https://github.com/fnando/gatefold)",
                env!("CARGO_PKG_VERSION")
            )
        })
    }

    pub fn lyrics_enabled(&self) -> bool {
        !self.cover_only && !self.artist_images_only
    }

    pub fn cover_enabled(&self) -> bool {
        !self.lyrics_only && !self.artist_images_only
    }

    /// `--lyrics-only` / `--cover-only` mean "only that", so they skip this pass too.
    pub fn artist_images_enabled(&self) -> bool {
        !self.skip_artist_images && !self.lyrics_only && !self.cover_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_hours(12));
        assert_eq!(parse_duration("30d").unwrap(), Duration::from_hours(720));
        assert_eq!(parse_duration("2w").unwrap(), Duration::from_hours(336));
        assert_eq!(parse_duration(" 1d ").unwrap(), Duration::from_hours(24));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["", "30", "d", "30x", "-1d", "1.5h", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn formats_with_largest_even_unit() {
        assert_eq!(format_duration(Duration::from_hours(720)), "30d");
        assert_eq!(format_duration(Duration::from_hours(336)), "2w");
        assert_eq!(format_duration(Duration::from_mins(90)), "90m");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn refresh_window_requires_replace() {
        let base = ["gatefold", "--state-dir", "/tmp/s"];
        assert!(
            Cli::try_parse_from(base.iter().chain(["--artist-refresh-after", "30d"].iter()))
                .is_err()
        );
        let cli = Cli::try_parse_from(
            base.iter()
                .chain(["--replace-artist-images", "--artist-refresh-after", "30d"].iter()),
        )
        .unwrap();
        assert_eq!(cli.artist_refresh_after, Some(Duration::from_hours(720)));
    }

    #[test]
    fn state_dir_is_required_for_a_run() {
        assert!(Cli::try_parse_from(["gatefold", "/music"]).is_err());
    }

    #[test]
    fn skip_needs_no_state_dir() {
        let cli = Cli::try_parse_from(["gatefold", "skip", "a/folder.jpg", "b/cover.jpg"]).unwrap();
        let Some(Command::Skip { images }) = cli.command else {
            panic!("expected the skip subcommand");
        };
        assert_eq!(
            images,
            [PathBuf::from("a/folder.jpg"), PathBuf::from("b/cover.jpg")]
        );
        assert!(Cli::try_parse_from(["gatefold", "skip"]).is_err());
    }

    #[test]
    fn restore_takes_either_a_dir_or_a_file() {
        let parse = |args: &[&str]| Cli::try_parse_from(["gatefold", "restore"].iter().chain(args));
        assert!(parse(&[]).is_err());
        assert!(parse(&["Kemuri"]).is_err(), "no positional path");
        assert!(parse(&["--dir", "a", "--file", "b"]).is_err());
        let Some(Command::Restore { dir, file, keep }) =
            parse(&["--dir", "Kemuri"]).unwrap().command
        else {
            panic!("expected the restore subcommand");
        };
        assert_eq!(
            (dir, file, keep),
            (Some(PathBuf::from("Kemuri")), None, false)
        );
        let Some(Command::Restore { keep, .. }) =
            parse(&["--dir", "Kemuri", "--keep"]).unwrap().command
        else {
            panic!("expected the restore subcommand");
        };
        assert!(keep);
        assert!(
            parse(&[
                "--file",
                "Kemuri/.gatefold-archive/primary/folder-2026-01-31.jpg"
            ])
            .is_ok()
        );
    }

    #[test]
    fn keep_takes_paths() {
        let cli = Cli::try_parse_from(["gatefold", "keep", "Kemuri", "Kemuri/folder.jpg"]).unwrap();
        let Some(Command::Keep { paths }) = cli.command else {
            panic!("expected the keep subcommand");
        };
        assert_eq!(paths.len(), 2);
        assert!(Cli::try_parse_from(["gatefold", "keep"]).is_err());
    }

    #[test]
    fn a_path_is_still_a_run() {
        let cli = Cli::try_parse_from(["gatefold", "/music", "--state-dir", "/tmp/s"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.path, PathBuf::from("/music"));
    }
}
