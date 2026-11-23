//! `$XDG_CONFIG_HOME/gatefold/config.toml` (or `--config` / `GATEFOLD_CONFIG`):
//! everything a run, and `gatefold run`, needs, so nothing has to be passed as a
//! flag. A missing file means the defaults.
//!
//! Precedence: flags, then environment variables (and `--env-file`), then the
//! config, then the defaults. Settings that have an environment variable are
//! applied by setting that variable when it isn't set already, so they keep
//! that order everywhere (subcommands included); the rest are applied to the
//! parsed [`Cli`] where the flag wasn't given.
//!
//! Secrets (`api_key`, `token`) may name a variable instead of holding the
//! value: `"$NAME"` or `"${NAME}"`.

use crate::cli::{Cli, parse_duration};
use anyhow::{Context, Result, bail};
use clap::ArgMatches;
use clap::parser::ValueSource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The example written by `gatefold config init`.
pub const EXAMPLE: &str = include_str!("../examples/config.toml");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub general: General,
    pub server: Server,
    pub lrclib: Lrclib,
    pub musicbrainz: MusicBrainz,
    pub jellyfin: Jellyfin,
    /// What `gatefold run` runs, each on its own interval and in parallel
    /// (never overlapping itself). Without any, it runs only when asked.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub jobs: BTreeMap<String, Job>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            general: General::default(),
            server: Server::default(),
            lrclib: Lrclib::default(),
            musicbrainz: MusicBrainz::default(),
            jellyfin: Jellyfin::default(),
            jobs: BTreeMap::new(),
        }
    }
}

/// One of `gatefold run`'s jobs: a run with its own schedule and options.
/// Unset options come from `[general]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // settings, as in the file
pub struct Job {
    /// How often it runs; unset: only when asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// Only items never looked up (`--new-only`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new_only: bool,
    /// Only items looked up before and not found (`--miss-only`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub miss_only: bool,
    /// `lyrics`, `covers` or `artist_images`: only that part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Only>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<Replace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_nfo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
    /// `false`: no artist images (`--skip-artist-images`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_images: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_videos: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_videos: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_missed_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_refresh_after: Option<String>,
}

/// What a job's `only` can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Only {
    Lyrics,
    Covers,
    ArtistImages,
}

impl Job {
    /// Its options on top of `cli` (the command line with `[general]` applied).
    pub fn apply(&self, cli: &mut Cli) {
        cli.new_only |= self.new_only;
        cli.miss_only |= self.miss_only;
        match self.only {
            Some(Only::Lyrics) => cli.lyrics_only = true,
            Some(Only::Covers) => cli.cover_only = true,
            Some(Only::ArtistImages) => cli.artist_images_only = true,
            None => {}
        }
        if let Some(replace) = &self.replace {
            cli.replace_lyrics = replace.includes(Replaceable::Lyrics);
            cli.replace_cover = replace.includes(Replaceable::Covers);
            cli.replace_artist_images = replace.includes(Replaceable::ArtistImages);
        }
        let set = |field: &mut bool, value: Option<bool>| {
            if let Some(value) = value {
                *field = value;
            }
        };
        set(&mut cli.write_nfo, self.write_nfo);
        set(&mut cli.verify, self.verify);
        set(
            &mut cli.skip_artist_images,
            self.artist_images.map(|on| !on),
        );
        set(
            &mut cli.skip_artist_videos,
            self.artist_videos.map(|on| !on),
        );
        set(&mut cli.skip_cover_videos, self.cover_videos.map(|on| !on));
        if let Some(after) = self.retry_missed_after.as_deref() {
            cli.retry_missed_after = parse_duration(after).ok();
        }
        if let Some(after) = self.artist_refresh_after.as_deref() {
            cli.artist_refresh_after = parse_duration(after).ok();
        }
        if !cli.replace_artist_images {
            cli.artist_refresh_after = None;
        }
    }

    fn validate(&self, name: &str) -> Result<()> {
        if self.new_only && self.miss_only {
            bail!("jobs.{name}: new_only and miss_only can't both be on");
        }
        if self.only == Some(Only::ArtistImages) && self.artist_images == Some(false) {
            bail!("jobs.{name}: only = \"artist_images\" with artist_images = false does nothing");
        }
        for (key, value) in [
            ("interval", &self.interval),
            ("retry_missed_after", &self.retry_missed_after),
            ("artist_refresh_after", &self.artist_refresh_after),
        ] {
            if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                parse_duration(value).map_err(|err| anyhow::anyhow!("jobs.{name}.{key}: {err}"))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // settings, as in the file
pub struct General {
    /// Music folders; each gets its own run.
    pub library: Vec<String>,
    /// Default: `$XDG_DATA_HOME/gatefold`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<String>,
    pub delay_ms: u64,
    /// Default: `gatefold/<version> (+repo)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub write_nfo: bool,
    /// Check every tag write with ffprobe before it replaces the file
    /// (`--verify`).
    pub verify: bool,
    /// Replace what's there when the source has something different:
    /// `true` for lyrics, covers and artist images, or a list of them
    /// (`--replace-lyrics`, `--replace-cover`, `--replace-artist-images`).
    /// Kept files stay.
    pub replace: Replace,
    pub artist_videos: bool,
    pub cover_videos: bool,
    /// `7d`, `12h`, …; unset: retry on every run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_missed_after: Option<String>,
    /// Used with `--replace-artist-images`; unset: always re-check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_refresh_after: Option<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            library: Vec::new(),
            state_dir: None,
            delay_ms: 300,
            user_agent: None,
            write_nfo: false,
            verify: false,
            replace: Replace::default(),
            artist_videos: true,
            cover_videos: true,
            retry_missed_after: None,
            artist_refresh_after: None,
        }
    }
}

/// `replace = true`, `false`, or a list of what to replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, try_from = "RawReplace")]
pub enum Replace {
    All(bool),
    Only(Vec<Replaceable>),
}

/// `replace` as written, before its names are checked.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawReplace {
    All(bool),
    Only(Vec<String>),
    Other(toml::Value),
}

impl TryFrom<RawReplace> for Replace {
    type Error = String;

    fn try_from(raw: RawReplace) -> Result<Self, Self::Error> {
        const EXPECTED: &str =
            "true, false, or a list of \"lyrics\", \"covers\", \"artist_images\"";
        match raw {
            RawReplace::All(all) => Ok(Self::All(all)),
            RawReplace::Only(names) => names
                .iter()
                .map(|name| match name.as_str() {
                    "lyrics" => Ok(Replaceable::Lyrics),
                    "covers" => Ok(Replaceable::Covers),
                    "artist_images" => Ok(Replaceable::ArtistImages),
                    other => Err(format!("replace: unknown \"{other}\"; expected {EXPECTED}")),
                })
                .collect::<Result<_, _>>()
                .map(Self::Only),
            RawReplace::Other(value) => Err(format!("replace: got {value}; expected {EXPECTED}")),
        }
    }
}

impl Default for Replace {
    fn default() -> Self {
        Self::All(false)
    }
}

/// What `replace` can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replaceable {
    Lyrics,
    Covers,
    ArtistImages,
}

impl Replace {
    pub fn includes(&self, what: Replaceable) -> bool {
        match self {
            Self::All(all) => *all,
            Self::Only(list) => list.contains(&what),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub enabled: bool,
    pub listen: String,
    /// Required when `listen` isn't a loopback address.
    pub token: String,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:8788".to_string(),
            token: "$GATEFOLD_SERVER_TOKEN".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lrclib {
    pub app_id: String,
    pub api_key: String,
}

impl Default for Lrclib {
    fn default() -> Self {
        Self {
            app_id: "$GATEFOLD_LRCLIB_APP_ID".to_string(),
            api_key: "$GATEFOLD_LRCLIB_API_KEY".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MusicBrainz {
    /// Default: `https://musicbrainz.org`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Jellyfin {
    pub url: String,
    pub api_key: String,
}

impl Default for Jellyfin {
    fn default() -> Self {
        Self {
            url: "$GATEFOLD_JELLYFIN_URL".to_string(),
            api_key: "$GATEFOLD_JELLYFIN_API_KEY".to_string(),
        }
    }
}

impl Config {
    /// Parse and check a config file's text.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Read `path`; a missing file is the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("reading {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "version = {} isn't one this gatefold reads (1)",
                self.version
            );
        }
        for (key, value) in [
            (
                "general.retry_missed_after",
                &self.general.retry_missed_after,
            ),
            (
                "general.artist_refresh_after",
                &self.general.artist_refresh_after,
            ),
        ] {
            if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                parse_duration(value).map_err(|err| anyhow::anyhow!("{key}: {err}"))?;
            }
        }
        for (name, job) in &self.jobs {
            if name.trim().is_empty() {
                bail!("jobs: a job needs a name ([jobs.<name>])");
            }
            job.validate(name)?;
        }
        if self.server.enabled {
            let addr: std::net::SocketAddr = self.server.listen.parse().with_context(|| {
                format!(
                    "server.listen: `{}` isn't an address:port",
                    self.server.listen
                )
            })?;
            if !addr.ip().is_loopback() && resolve(&self.server.token).is_none() {
                bail!(
                    "server.token is required when server.listen ({}) isn't a loopback address",
                    self.server.listen
                );
            }
        }
        Ok(())
    }

    /// The library folders, with `~` expanded.
    pub fn library(&self) -> Vec<PathBuf> {
        self.general.library.iter().map(|dir| expand(dir)).collect()
    }

    /// `gatefold run`'s jobs, each with its interval (`None`: only when
    /// asked).
    pub fn jobs(&self) -> Vec<(String, Job, Option<std::time::Duration>)> {
        self.jobs
            .iter()
            .map(|(name, job)| {
                let every = job
                    .interval
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .and_then(|v| parse_duration(v).ok());
                (name.clone(), job.clone(), every)
            })
            .collect()
    }

    /// The configured state dir, with `~` expanded.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.general
            .state_dir
            .as_deref()
            .filter(|dir| !dir.is_empty())
            .map(expand)
    }

    /// The token to sign in to the web UI, when one is set.
    pub fn token(&self) -> Option<String> {
        resolve(&self.server.token)
    }

    /// The settings that have an environment variable, as `(variable, value)`,
    /// secrets resolved; empty values are left out.
    fn env(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("GATEFOLD_USER_AGENT", self.general.user_agent.clone()),
            ("GATEFOLD_LRCLIB_APP_ID", resolve(&self.lrclib.app_id)),
            ("GATEFOLD_LRCLIB_API_KEY", resolve(&self.lrclib.api_key)),
            ("GATEFOLD_MUSICBRAINZ_URL", self.musicbrainz.url.clone()),
            ("GATEFOLD_JELLYFIN_URL", resolve(&self.jellyfin.url)),
            ("GATEFOLD_JELLYFIN_API_KEY", resolve(&self.jellyfin.api_key)),
        ];
        vars.retain(|(_, value)| value.as_deref().is_some_and(|v| !v.is_empty()));
        vars.into_iter()
            .filter_map(|(name, value)| Some((name, value?)))
            .collect()
    }

    /// Fill in what the command line didn't say. `matches` tells flags given
    /// (or set through their environment variable) from defaults.
    pub fn apply(&self, cli: &mut Cli, matches: &ArgMatches) {
        let given = |id: &str| {
            matches!(
                matches.value_source(id),
                Some(ValueSource::CommandLine | ValueSource::EnvVariable)
            )
        };
        if !given("dir")
            && let Some(dir) = self.library().into_iter().next()
        {
            cli.dir = dir;
        }
        if !given("state_dir") {
            cli.state_dir = cli.state_dir.take().or_else(|| self.state_dir());
        }
        if !given("delay_ms") {
            cli.delay_ms = self.general.delay_ms;
        }
        cli.write_nfo |= self.general.write_nfo;
        cli.verify |= self.general.verify;
        let replace = &self.general.replace;
        cli.replace_lyrics |= replace.includes(Replaceable::Lyrics);
        cli.replace_cover |= replace.includes(Replaceable::Covers);
        cli.replace_artist_images |= replace.includes(Replaceable::ArtistImages);
        cli.skip_artist_videos |= !self.general.artist_videos;
        cli.skip_cover_videos |= !self.general.cover_videos;
        if !given("retry_missed_after") && cli.retry_missed_after.is_none() {
            cli.retry_missed_after = self
                .general
                .retry_missed_after
                .as_deref()
                .and_then(|v| parse_duration(v).ok());
        }
        // Only meaningful with --replace-artist-images (clap requires it there).
        if !given("artist_refresh_after")
            && cli.artist_refresh_after.is_none()
            && cli.replace_artist_images
        {
            cli.artist_refresh_after = self
                .general
                .artist_refresh_after
                .as_deref()
                .and_then(|v| parse_duration(v).ok());
        }
    }

    /// This config as TOML with every secret masked, for `gatefold config`.
    pub fn masked(&self) -> String {
        let mut shown = self.clone();
        for secret in [
            &mut shown.lrclib.api_key,
            &mut shown.jellyfin.api_key,
            &mut shown.server.token,
        ] {
            if !secret.is_empty() && env_name(secret).is_none() {
                *secret = "********".to_string();
            }
        }
        toml::to_string_pretty(&shown).unwrap_or_default()
    }
}

/// Set the environment variables this config provides, unless they're set
/// already. Call before parsing the command line, while single-threaded.
pub fn export_env(config: &Config) {
    for (name, value) in config.env() {
        if std::env::var_os(name).is_none_or(|v| v.is_empty()) {
            // SAFETY: called from `main` before any thread is started.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// The config file: `--config`, else `GATEFOLD_CONFIG`, else
/// `$XDG_CONFIG_HOME/gatefold/config.toml` (or `~/.config/gatefold/config.toml`).
pub fn path(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| {
        std::env::var_os("GATEFOLD_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .or_else(|| {
        default_path(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    })
}

fn default_path(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("gatefold").join("config.toml"))
}

static LOADED: OnceLock<(Option<PathBuf>, Config)> = OnceLock::new();

/// Load the config once, for this process. Later calls return the same.
pub fn init(flag: Option<PathBuf>) -> Result<&'static Config> {
    if let Some((_, config)) = LOADED.get() {
        return Ok(config);
    }
    let path = path(flag);
    let config = match &path {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    Ok(&LOADED.get_or_init(|| (path, config)).1)
}

/// The config loaded by [`init`] (the defaults before that).
pub fn get() -> &'static Config {
    static DEFAULT: OnceLock<Config> = OnceLock::new();
    LOADED.get().map_or_else(
        || DEFAULT.get_or_init(Config::default),
        |(_, config)| config,
    )
}

/// The file [`init`] read (or would have).
pub fn loaded_path() -> Option<&'static Path> {
    LOADED.get().and_then(|(path, _)| path.as_deref())
}

/// `~/x` → `$HOME/x`.
pub fn expand(value: &str) -> PathBuf {
    match (value.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if value == "~" => std::env::var_os("HOME").map_or_else(|| value.into(), PathBuf::from),
        _ => PathBuf::from(value),
    }
}

/// The variable a secret refers to, when the whole value is `$NAME` or
/// `${NAME}` (letters, digits and `_`, not starting with a digit).
pub fn env_name(value: &str) -> Option<&str> {
    let name = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

/// A secret's value: the variable's when it names one (`None` if unset or
/// empty), else the value itself (`None` if empty).
pub fn resolve(value: &str) -> Option<String> {
    resolve_with(value, |name| std::env::var(name).ok())
}

fn resolve_with(value: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    match env_name(value) {
        Some(name) => var(name).filter(|v| !v.is_empty()),
        None => (!value.is_empty()).then(|| value.to_string()),
    }
}

/// Scan argv for the last `--config <PATH>` / `--config=<PATH>`, before clap
/// runs (the config decides what some arguments default to).
pub fn flag_from_args(args: impl IntoIterator<Item = String>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    let mut found = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--config=") {
            found = Some(value.into());
        } else if arg == "--config" {
            found = args.next().map(Into::into);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::{CommandFactory, FromArgMatches};

    fn parse_cli(args: &[&str]) -> (Cli, ArgMatches) {
        let matches = Cli::command()
            .try_get_matches_from(std::iter::once("gatefold").chain(args.iter().copied()))
            .unwrap();
        (Cli::from_arg_matches(&matches).unwrap(), matches)
    }

    #[test]
    fn the_example_is_valid() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.jobs().len(), 3);
        // Secrets and URLs come from the environment unless the file says so.
        assert_eq!(
            Config::default().jellyfin.api_key,
            "$GATEFOLD_JELLYFIN_API_KEY"
        );
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert!(Config::parse("version = 2").is_err());
        assert!(Config::parse("[general]\nnope = 1").is_err());
        assert!(
            Config::parse("[schedule]\ninterval = \"6h\"").is_err(),
            "no such section"
        );
    }

    #[test]
    fn a_public_listen_address_needs_a_token() {
        let open = "[server]\nlisten = \"0.0.0.0:8788\"";
        assert!(Config::parse(open).is_err());
        assert!(Config::parse(&format!("{open}\ntoken = \"s3cret\"")).is_ok());
        assert!(Config::parse("[server]\nenabled = false\nlisten = \"0.0.0.0:1\"").is_ok());
    }

    #[test]
    fn the_config_fills_in_what_flags_dont_say() {
        let config = Config::parse(
            "[general]
             library = [\"/music\", \"/more\"]
             delay_ms = 500
             write_nfo = true
             verify = true
             replace = true
             cover_videos = false
             artist_refresh_after = \"30d\"
             retry_missed_after = \"7d\"",
        )
        .unwrap();

        let (mut cli, matches) = parse_cli(&[]);
        config.apply(&mut cli, &matches);
        assert_eq!(cli.dir, PathBuf::from("/music"));
        assert_eq!(cli.delay_ms, 500);
        assert!(cli.write_nfo);
        assert!(cli.skip_cover_videos);
        assert!(cli.verify);
        assert!(cli.replace_lyrics && cli.replace_cover && cli.replace_artist_images);
        assert_eq!(
            cli.artist_refresh_after,
            Some(std::time::Duration::from_hours(720))
        );
        assert!(!cli.skip_artist_videos);
        assert_eq!(
            cli.retry_missed_after,
            Some(std::time::Duration::from_hours(168))
        );

        // Flags win.
        let (mut cli, matches) = parse_cli(&[
            "--dir",
            "/tmp",
            "--delay-ms",
            "100",
            "--retry-missed-after",
            "1h",
        ]);
        config.apply(&mut cli, &matches);
        assert_eq!(cli.dir, PathBuf::from("/tmp"));
        assert_eq!(cli.delay_ms, 100);
        assert_eq!(
            cli.retry_missed_after,
            Some(std::time::Duration::from_hours(1))
        );
    }

    #[test]
    fn replace_takes_true_or_a_list() {
        let flags = |value: &str| {
            let config = Config::parse(&format!("[general]\nreplace = {value}")).unwrap();
            let (mut cli, matches) = parse_cli(&[]);
            config.apply(&mut cli, &matches);
            (
                cli.replace_lyrics,
                cli.replace_cover,
                cli.replace_artist_images,
            )
        };
        assert_eq!(flags("true"), (true, true, true));
        assert_eq!(flags("false"), (false, false, false));
        assert_eq!(flags(r#"["covers", "artist_images"]"#), (false, true, true));
        assert_eq!(flags("[]"), (false, false, false));
        let err = Config::parse("[general]\nreplace = [\"everything\"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown \"everything\""), "{err}");
        assert!(Config::parse("[general]\nreplace = \"covers\"").is_err());
    }

    #[test]
    fn jobs_run_with_their_own_options() {
        let config = Config::parse(
            "[general]
             verify = true
             replace = true

             [jobs.new-tracks]
             interval = \"5m\"
             new_only = true
             write_nfo = true

             [jobs.artist-images]
             only = \"artist_images\"
             replace = [\"artist_images\"]
             artist_refresh_after = \"1d\"

             [jobs.missed]
             miss_only = true
             retry_missed_after = \"1d\"
             artist_images = false
             verify = false",
        )
        .unwrap();
        let jobs = config.jobs();
        let names: Vec<&str> = jobs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["artist-images", "missed", "new-tracks"]);
        let every = |name: &str| jobs.iter().find(|(n, _, _)| n == name).unwrap().2;
        assert_eq!(every("new-tracks"), Some(std::time::Duration::from_mins(5)));
        assert_eq!(every("missed"), None, "only when asked");

        let cli_for = |name: &str| {
            let (mut cli, matches) = parse_cli(&[]);
            config.apply(&mut cli, &matches);
            config.jobs[name].apply(&mut cli);
            cli
        };
        let new = cli_for("new-tracks");
        assert!(new.new_only && new.write_nfo && new.verify && new.replace_cover);
        let artists = cli_for("artist-images");
        assert!(artists.artist_images_only && artists.replace_artist_images);
        assert!(!artists.replace_cover && !artists.replace_lyrics);
        assert_eq!(
            artists.artist_refresh_after,
            Some(std::time::Duration::from_hours(24))
        );
        let missed = cli_for("missed");
        assert!(missed.miss_only && missed.skip_artist_images && !missed.verify);
        assert_eq!(
            missed.retry_missed_after,
            Some(std::time::Duration::from_hours(24))
        );

        assert!(Config::parse("[jobs.x]\nnew_only = true\nmiss_only = true").is_err());
        assert!(Config::parse("[jobs.x]\ninterval = \"soon\"").is_err());
        assert!(Config::parse("[jobs.x]\nnope = 1").is_err());
        // Without jobs, nothing runs on its own.
        assert_eq!(Config::default().jobs().len(), 0);
    }

    #[test]
    fn secrets_can_name_a_variable() {
        assert_eq!(env_name("$KEY"), Some("KEY"));
        assert_eq!(env_name("${KEY_2}"), Some("KEY_2"));
        for literal in ["abc", "$", "${}", "$2X", "a$KEY", "$KEY-1"] {
            assert_eq!(env_name(literal), None, "{literal}");
        }
        let var = |name: &str| (name == "SET").then(|| "value".to_string());
        assert_eq!(resolve_with("$SET", var), Some("value".into()));
        assert_eq!(resolve_with("$UNSET", var), None);
        assert_eq!(resolve_with("plain", var), Some("plain".into()));
        assert_eq!(resolve_with("", var), None);
    }

    #[test]
    fn masks_literal_secrets_only() {
        let mut config = Config::default();
        config.lrclib.api_key = "abc".into();
        config.jellyfin.api_key = "$GATEFOLD_JELLYFIN_API_KEY".into();
        let shown = config.masked();
        assert!(shown.contains("api_key = \"********\""), "{shown}");
        assert!(shown.contains("$GATEFOLD_JELLYFIN_API_KEY"), "{shown}");
        assert!(!shown.contains("abc"));
    }

    #[test]
    fn finds_the_file() {
        assert_eq!(
            default_path(Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/x/gatefold/config.toml"))
        );
        assert_eq!(
            default_path(None, Some("/h".into())),
            Some(PathBuf::from("/h/.config/gatefold/config.toml"))
        );
        assert_eq!(
            flag_from_args(
                ["--dir", ".", "--config=/a.toml", "--config", "/b.toml"].map(String::from)
            ),
            Some(PathBuf::from("/b.toml"))
        );
    }
}
