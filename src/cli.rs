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
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: PathBuf,

    /// Base directory for cached API responses (`lrc/`, `artist/`, `release/`),
    /// downloaded images (`art/`), and metadata (`metadata/`). Defaults to
    /// `$XDG_DATA_HOME/gatefold` (or `~/.local/share/gatefold`).
    #[arg(long, value_name = "DIR")]
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
    /// `GATEFOLD_MUSICBRAINZ_URL` / `GATEFOLD_JELLYFIN_URL` / `GATEFOLD_JELLYFIN_API_KEY`
    /// from a `KEY=VALUE` env file, keeping secrets out of
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
    /// `.gatefold-archive/`. Images you keep (`gatefold keep`) stay, but Apple's
    /// differing ones are archived there too, for `gatefold restore`.
    #[arg(long, conflicts_with_all = ["skip_artist_images", "lyrics_only", "cover_only"])]
    pub replace_artist_images: bool,

    /// Use this Apple Music artist page (e.g.
    /// `https://music.apple.com/us/artist/dead-fish/123456`) for the one album
    /// artist under --dir, when `MusicBrainz` and Wikidata have no (or the wrong)
    /// link. Remembered for later runs; this run replaces the artist's images as
    /// --replace-artist-images would.
    #[arg(
        long,
        value_name = "URL",
        value_parser = parse_apple_url,
        conflicts_with_all = ["skip_artist_images", "lyrics_only", "cover_only"]
    )]
    pub apple_url: Option<String>,

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

    #[command(flatten)]
    pub jellyfin: JellyfinArgs,
}

/// The Jellyfin server whose artist images are kept in line with the artist
/// folders. Both are needed; without them nothing talks to Jellyfin.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct JellyfinArgs {
    /// Jellyfin server URL (e.g. `http://192.168.1.10:8096`). After the artist
    /// pass, Jellyfin is made to show each artist folder's `folder.jpg`,
    /// `logo.png` and `backdrop.jpg`, with that backdrop first.
    #[arg(long, value_name = "URL", env = "GATEFOLD_JELLYFIN_URL")]
    pub jellyfin_url: Option<String>,

    /// Jellyfin API key (Dashboard → API Keys). Prefer --env-file: a flag value
    /// is visible in `ps`.
    #[arg(
        long,
        value_name = "KEY",
        env = "GATEFOLD_JELLYFIN_API_KEY",
        hide_env_values = true
    )]
    pub jellyfin_api_key: Option<String>,
}

impl JellyfinArgs {
    /// The URL and key, when both are set.
    pub fn server(&self) -> Option<(&str, &str)> {
        Some((
            self.jellyfin_url.as_deref().filter(|u| !u.is_empty())?,
            self.jellyfin_api_key.as_deref().filter(|k| !k.is_empty())?,
        ))
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Reject an image you don't want, but stay open to a different one: writes a
    /// `.gatefold-<sha256>-skip` marker next to it. Later runs replace the image as
    /// soon as another one is available and never write these exact contents
    /// again. Delete the marker to undo.
    Skip {
        /// Album covers (`cover.jpg`) or artist files (`folder.jpg`,
        /// `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`). Repeat for
        /// several (`--file a --file b`).
        #[arg(long = "file", value_name = "IMAGE", required = true)]
        images: Vec<PathBuf>,
    },
    /// Never replace an image, even with `--replace-cover` /
    /// `--replace-artist-images`: writes the keep marker for its kind
    /// (`.gatefold-primary-keep`, `.gatefold-cover-keep`, …), or `.gatefold-keep`
    /// in a folder, keeping everything it governs. A skip of the image is taken
    /// back. Delete the marker to undo.
    Keep {
        /// Album covers (`cover.jpg`) or artist files (`folder.jpg`,
        /// `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`). Repeat for
        /// several.
        #[arg(long = "file", value_name = "IMAGE", required_unless_present = "dirs")]
        files: Vec<PathBuf>,

        /// Artist or album folders, to keep everything they govern. Repeat
        /// for several.
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
    },
    /// Move an artist file into its artist folder's `.gatefold-archive/`,
    /// dated, as `--replace-artist-images` would. The file is skipped first
    /// (see `skip`), so later runs write a different image once one is
    /// available, never this one. `restore` brings it back.
    Archive {
        /// Artist files (`folder.jpg`, `backdrop.jpg`, `backdrops/backdrop.mp4`,
        /// `logo.png`). Repeat for several.
        #[arg(long = "file", value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Shrink the images and motion backdrops already in a library, as gatefold
    /// does with everything it downloads: artist files and their
    /// `.gatefold-archive/`, album covers (`cover.*`) and the front covers
    /// embedded in tracks. Images are optimized losslessly (pixels never change;
    /// JPEGs keep their Exif and colour profile); motion backdrops still as Apple
    /// served them get the 1080p x264 re-encode (needs ffmpeg with libx264; HDR
    /// and already re-encoded ones are left alone). Kept files (see `keep`) are
    /// left as they are; skipped ones stay skipped.
    Optimize {
        /// Folders to walk (a whole library, an artist, an album). Repeat for
        /// several.
        #[arg(long = "dir", value_name = "DIR", required = true)]
        dirs: Vec<PathBuf>,

        /// Report what would be saved without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Validate each track's new tag with ffprobe before it replaces the
        /// original (see the run's `--verify`). Requires ffprobe (ffmpeg).
        #[arg(long)]
        verify: bool,
    },
    /// Keep Jellyfin's artist images and the artist folders in line: `push`
    /// makes Jellyfin show the folders' images, `pull` brings the images only
    /// Jellyfin has into the folders.
    Jellyfin {
        #[command(subcommand)]
        action: JellyfinCommand,
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

/// `gatefold jellyfin` actions.
#[derive(Debug, Subcommand)]
pub enum JellyfinCommand {
    /// Make Jellyfin show each artist folder's own images: `folder.*` as its
    /// photo, `logo.*` as its logo, and `backdrop.*` as its first backdrop.
    /// Older backdrops Jellyfin stored itself are kept, after ours (they're
    /// deleted and uploaded again, as Jellyfin has no safe way to reorder).
    /// Photos and logos Jellyfin stored itself are replaced by the folder's.
    /// Anything deleted is saved first under the state dir's
    /// `backup/jellyfin/`. Nothing in the artist folders is changed. A run
    /// with the Jellyfin server set does this after its artist pass.
    Push(JellyfinTarget),
    /// Copy the artist images only Jellyfin has (from its metadata providers
    /// or uploads) into the artist folders: written when the folder has none
    /// of that kind, otherwise archived, dated, in `.gatefold-archive/` for
    /// `gatefold restore`. Existing files are never replaced, and Jellyfin
    /// isn't changed: run `push` next so it shows the folders' files. JPEG and
    /// PNG only; other formats stay in Jellyfin.
    Pull(JellyfinTarget),
}

/// What `gatefold jellyfin push` / `pull` work on.
#[derive(Debug, Clone, clap::Args)]
pub struct JellyfinTarget {
    /// Artist folders, or a library whose subfolders are artist folders.
    /// Repeat for several.
    #[arg(long = "dir", value_name = "DIR", required = true)]
    pub dirs: Vec<PathBuf>,

    /// Report what would change without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Base directory for gatefold's state (see the main command).
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Load `GATEFOLD_JELLYFIN_URL` / `GATEFOLD_JELLYFIN_API_KEY` from a
    /// `KEY=VALUE` env file (see the main command).
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    #[command(flatten)]
    pub jellyfin: JellyfinArgs,
}

/// `30d`, `12h`, `90m`, `45s`, `2w`: a whole number plus a unit.
/// An Apple Music artist URL → its numeric artist id.
fn parse_apple_url(text: &str) -> Result<String, String> {
    crate::musicbrainz::apple_artist_id(text).ok_or_else(|| {
        "expected an Apple Music artist URL, like https://music.apple.com/us/artist/name/123456"
            .to_string()
    })
}

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

/// The `User-Agent` gatefold sends unless told otherwise.
pub fn default_user_agent() -> String {
    format!(
        "gatefold/{} (https://github.com/fnando/gatefold)",
        env!("CARGO_PKG_VERSION")
    )
}

impl Cli {
    /// Resolve the `User-Agent`, falling back to a descriptive default.
    pub fn user_agent(&self) -> String {
        self.user_agent.clone().unwrap_or_else(default_user_agent)
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
    fn jellyfin_server_is_set_on_the_run_and_the_subcommands() {
        let server = [
            "--jellyfin-url",
            "http://jf:8096",
            "--jellyfin-api-key",
            "k",
        ];
        let run = Cli::try_parse_from(["gatefold"].iter().chain(server.iter())).unwrap();
        assert_eq!(run.jellyfin.server(), Some(("http://jf:8096", "k")));

        for action in ["push", "pull"] {
            let args = [
                "gatefold",
                "jellyfin",
                action,
                "--dir",
                "/music",
                "--dry-run",
            ];
            let sub = Cli::try_parse_from(args.iter().chain(server.iter())).unwrap();
            let Some(Command::Jellyfin { action: parsed }) = sub.command else {
                panic!("expected the jellyfin subcommand");
            };
            let (JellyfinCommand::Push(target) | JellyfinCommand::Pull(target)) = &parsed;
            assert_eq!(matches!(parsed, JellyfinCommand::Pull(_)), action == "pull");
            assert!(target.dry_run);
            assert_eq!(target.jellyfin.server(), Some(("http://jf:8096", "k")));
        }
        assert_eq!(JellyfinArgs::default().server(), None);
    }

    #[test]
    fn apple_url_takes_an_apple_music_artist_url() {
        let cli = Cli::try_parse_from([
            "gatefold",
            "--apple-url",
            "https://music.apple.com/br/artist/dead-fish/1234567",
        ])
        .unwrap();
        assert_eq!(cli.apple_url.as_deref(), Some("1234567"));
        for bad in [
            "https://open.spotify.com/artist/7Lvg39k5XgXevGR767ikYI",
            "https://music.apple.com/br/album/afasia/1234567",
        ] {
            assert!(Cli::try_parse_from(["gatefold", "--apple-url", bad]).is_err());
        }
        assert!(
            Cli::try_parse_from([
                "gatefold",
                "--lyrics-only",
                "--apple-url",
                "https://music.apple.com/us/artist/x/1"
            ])
            .is_err()
        );
    }

    #[test]
    fn state_dir_is_optional_for_a_run() {
        let cli = Cli::try_parse_from(["gatefold", "--dir", "/music"]).unwrap();
        assert!(cli.state_dir.is_none());
    }

    #[test]
    fn skip_needs_no_state_dir() {
        let cli = Cli::try_parse_from([
            "gatefold",
            "skip",
            "--file",
            "a/folder.jpg",
            "--file",
            "b/cover.jpg",
        ])
        .unwrap();
        let Some(Command::Skip { images }) = cli.command else {
            panic!("expected the skip subcommand");
        };
        assert_eq!(
            images,
            [PathBuf::from("a/folder.jpg"), PathBuf::from("b/cover.jpg")]
        );
        assert!(Cli::try_parse_from(["gatefold", "skip"]).is_err());
        assert!(
            Cli::try_parse_from(["gatefold", "skip", "a/folder.jpg"]).is_err(),
            "no positional images"
        );
        assert!(
            Cli::try_parse_from(["gatefold", "skip", "--file", "a", "b"]).is_err(),
            "one image per --file"
        );
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
    fn keep_takes_files_and_dirs() {
        let parse = |args: &[&str]| Cli::try_parse_from(["gatefold", "keep"].iter().chain(args));
        let Some(Command::Keep { files, dirs }) = parse(&[
            "--dir",
            "Kemuri",
            "--file",
            "Kemuri/folder.jpg",
            "--file",
            "Kemuri/logo.png",
        ])
        .unwrap()
        .command
        else {
            panic!("expected the keep subcommand");
        };
        assert_eq!(files.len(), 2);
        assert_eq!(dirs, [PathBuf::from("Kemuri")]);
        assert!(parse(&["--dir", "Kemuri"]).is_ok());
        assert!(parse(&[]).is_err());
        assert!(parse(&["Kemuri"]).is_err(), "no positional paths");
        assert!(parse(&["--dir", "a", "b"]).is_err(), "one folder per --dir");
    }

    #[test]
    fn archive_takes_one_file_per_flag() {
        let parse = |args: &[&str]| Cli::try_parse_from(["gatefold", "archive"].iter().chain(args));
        let Some(Command::Archive { files }) =
            parse(&["--file", "Kemuri/backdrop.jpg", "--file", "Kemuri/logo.png"])
                .unwrap()
                .command
        else {
            panic!("expected the archive subcommand");
        };
        assert_eq!(files.len(), 2);
        assert!(parse(&[]).is_err());
        assert!(parse(&["--file", "a", "b"]).is_err(), "one file per --file");
    }

    #[test]
    fn a_run_scans_dir() {
        let cli =
            Cli::try_parse_from(["gatefold", "--dir", "/music", "--state-dir", "/tmp/s"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.dir, PathBuf::from("/music"));
        let cli = Cli::try_parse_from(["gatefold", "--state-dir", "/tmp/s"]).unwrap();
        assert_eq!(cli.dir, PathBuf::from("."));
        assert!(
            Cli::try_parse_from(["gatefold", "/music", "--state-dir", "/tmp/s"]).is_err(),
            "no positional folder"
        );
    }
}
