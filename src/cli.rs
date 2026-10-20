use clap::Parser;
use std::path::PathBuf;

/// Scan a folder of music files and backfill lyrics from lrclib.net.
#[derive(Debug, Parser)]
#[command(name = "gatefold", version, about)]
#[allow(clippy::struct_excessive_bools)] // CLI flags are naturally independent bools
pub struct Cli {
    /// Folder to scan recursively for music files.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Base directory for cached API responses (`lrc/`) and metadata (`metadata/`).
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

    /// Load `GATEFOLD_LRCLIB_API_KEY` / `GATEFOLD_LRCLIB_APP_ID` / `GATEFOLD_USER_AGENT`
    /// from a `KEY=VALUE` env file, keeping secrets out of the process args (and `ps`).
    /// Flags and real environment variables override the file. Exits with an error if
    /// the file can't be read.
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    /// Override the `User-Agent` sent to lrclib.net and the Cover Art Archive.
    #[arg(long, value_name = "UA", env = "GATEFOLD_USER_AGENT")]
    pub user_agent: Option<String>,

    /// Delay between API requests in milliseconds (docs recommend 200-500).
    #[arg(long, value_name = "MS", default_value_t = 300)]
    pub delay_ms: u64,

    /// Populate the caches (lookups + downloads) but make no changes to music files.
    #[arg(long)]
    pub cache_only: bool,

    /// Replace an existing embedded lyrics tag instead of keeping it. The previous
    /// value is snapshotted under the state dir's `backup/` tree so it can be restored.
    #[arg(long)]
    pub replace_lyrics: bool,

    /// Replace an existing embedded cover instead of keeping it. The previous value
    /// is snapshotted under the state dir's `backup/` tree so it can be restored.
    #[arg(long)]
    pub replace_cover: bool,

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

    /// Only process lyrics; skip cover art entirely.
    #[arg(long, conflicts_with = "cover_only")]
    pub lyrics_only: bool,

    /// Only process cover art; skip lyrics entirely.
    #[arg(long)]
    pub cover_only: bool,
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
}
