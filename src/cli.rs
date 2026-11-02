use clap::Parser;
use std::path::PathBuf;

/// Scan a folder of music files and backfill lyrics, cover art, and artist images.
#[derive(Debug, Parser)]
#[command(name = "gatefold", version, about)]
#[allow(clippy::struct_excessive_bools)] // CLI flags are naturally independent bools
pub struct Cli {
    /// Folder to scan recursively for music files.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Base directory for cached API responses (`lrc/`, `artist/`, `release/`),
    /// downloaded images (`art/`), and metadata (`metadata/`).
    #[arg(long, value_name = "DIR")]
    pub state_dir: PathBuf,

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

    /// Replace an artist folder's `folder.jpg` / `backdrop.jpg` / motion backdrop
    /// when the artist's image has changed since it was written. The previous files
    /// are kept, dated, in `<artist>/primary/` and `<artist>/backdrops/`.
    #[arg(long, conflicts_with_all = ["skip_artist_images", "lyrics_only", "cover_only"])]
    pub replace_artist_images: bool,

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
