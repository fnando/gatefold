//! Jellyfin: make an artist folder's own images the ones Jellyfin shows.
//!
//! Jellyfin adds a backdrop it finds on disk after the ones it already has, so
//! an older backdrop it stored itself (from a metadata provider or an upload)
//! can stay first. Its reorder call swaps the image *files* — it would
//! overwrite `backdrop.jpg` with the older image — so the backdrops ahead of
//! ours are deleted and uploaded again instead, which puts them after it.
//! Nothing in the artist folder is ever deleted: only images Jellyfin keeps in
//! its own metadata folder are.

use crate::revision::{self, Revisions};
use crate::state::digest_bytes;
use crate::writer::ArtistImage;
use crate::{lossless, net, skip};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;
use ureq::http::Response;
use ureq::{Agent, Body};

const IMAGE_LIMIT: u64 = 64 * 1024 * 1024;
const JSON_LIMIT: u64 = 64 * 1024 * 1024;
/// Without WebP and AVIF here, Jellyfin converts those images to PNG.
const ACCEPT_IMAGES: &str = "image/webp,image/avif,image/*,*/*";

/// The artist images gatefold writes, by Jellyfin image type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Logo,
    Backdrop,
}

impl Kind {
    const ALL: [Self; 3] = [Self::Primary, Self::Logo, Self::Backdrop];

    fn api_name(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Logo => "Logo",
            Self::Backdrop => "Backdrop",
        }
    }

    /// The artist folder's file name, without its extension.
    fn stem(self) -> &'static str {
        match self {
            Self::Primary => "folder",
            Self::Logo => "logo",
            Self::Backdrop => "backdrop",
        }
    }

    /// The artist folder file this image is.
    fn artist_image(self) -> ArtistImage {
        match self {
            Self::Primary => ArtistImage::Avatar,
            Self::Logo => ArtistImage::Logo,
            Self::Backdrop => ArtistImage::Backdrop,
        }
    }

    fn from_api(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.api_name() == name)
    }
}

/// A Jellyfin album artist.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub provider_ids: HashMap<String, String>,
}

impl Artist {
    fn folder_name(&self) -> Option<&str> {
        self.path
            .as_deref()?
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
    }

    fn mbid(&self) -> Option<&str> {
        self.provider_ids
            .get("MusicBrainzArtist")
            .map(String::as_str)
    }
}

/// One of an item's images, as `/Items/{id}/Images` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInfo {
    pub image_type: String,
    #[serde(default)]
    pub image_index: Option<u32>,
    #[serde(default)]
    pub path: String,
}

impl ImageInfo {
    fn kind(&self) -> Option<Kind> {
        Kind::from_api(&self.image_type)
    }
}

/// The calls [`sync`] makes, so it can be tested without a server.
pub trait Api {
    fn images(&self, item: &str) -> Result<Vec<ImageInfo>>;
    fn download(&self, item: &str, kind: Kind, index: Option<u32>) -> Result<Vec<u8>>;
    fn delete(&self, item: &str, kind: Kind, index: Option<u32>) -> Result<()>;
    fn upload(&self, item: &str, kind: Kind, data: &[u8]) -> Result<()>;
    /// Have Jellyfin look at the item's folder again, without fetching anything.
    fn refresh(&self, item: &str) -> Result<()>;
}

/// A Jellyfin server, reached with an API key.
pub struct Client {
    agent: Agent,
    base: String,
    auth: String,
    user_agent: String,
}

impl Client {
    pub fn new(url: &str, api_key: &str, user_agent: String) -> Self {
        Self {
            agent: net::agent(),
            base: url.trim_end_matches('/').to_string(),
            auth: format!("MediaBrowser Token=\"{api_key}\""),
            user_agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn get(&self, path: &str, accept: &str) -> Result<Response<Body>> {
        let response = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.auth.as_str())
            .header("User-Agent", self.user_agent.as_str())
            .header("Accept", accept)
            .call()
            .map_err(|e| anyhow!("Jellyfin request failed: {e}"))?;
        ok(response, path)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut response = self.get(path, "application/json")?;
        let text = net::read_text(&mut response, JSON_LIMIT)?;
        serde_json::from_str(&text).with_context(|| format!("reading Jellyfin's {path}"))
    }

    /// Every album artist, with its folder and provider ids.
    pub fn artists(&self) -> Result<Vec<Artist>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Page {
            items: Vec<Artist>,
        }
        let page: Page = self.get_json("/Artists/AlbumArtists?Fields=Path,ProviderIds")?;
        Ok(page.items)
    }

    /// The music libraries' folders, and whether any of them saves artwork into
    /// the media folders (where an upload would then land).
    pub fn music_libraries(&self) -> Result<(Vec<String>, bool)> {
        let folders: Vec<Value> = self.get_json("/Library/VirtualFolders")?;
        let music = folders
            .iter()
            .filter(|f| f["CollectionType"].as_str() == Some("music"));
        let mut locations = Vec::new();
        let mut saves_locally = false;
        for library in music {
            saves_locally |= library["LibraryOptions"]["SaveLocalMetadata"].as_bool() == Some(true);
            if let Some(list) = library["Locations"].as_array() {
                locations.extend(list.iter().filter_map(Value::as_str).map(String::from));
            }
        }
        Ok((locations, saves_locally))
    }
}

fn ok(response: Response<Body>, what: &str) -> Result<Response<Body>> {
    let status = response.status();
    if !status.is_success() {
        bail!("Jellyfin answered {} for {what}", status.as_u16());
    }
    Ok(response)
}

fn image_path(item: &str, kind: Kind, index: Option<u32>) -> String {
    let index = index.map_or_else(String::new, |i| format!("/{i}"));
    format!("/Items/{item}/Images/{}{index}", kind.api_name())
}

impl Api for Client {
    fn images(&self, item: &str) -> Result<Vec<ImageInfo>> {
        self.get_json(&format!("/Items/{item}/Images"))
    }

    fn download(&self, item: &str, kind: Kind, index: Option<u32>) -> Result<Vec<u8>> {
        let path = image_path(item, kind, index);
        let mut response = self.get(&path, ACCEPT_IMAGES)?;
        net::read_limited(&mut response, IMAGE_LIMIT)
    }

    fn delete(&self, item: &str, kind: Kind, index: Option<u32>) -> Result<()> {
        let path = image_path(item, kind, index);
        let response = self
            .agent
            .delete(self.url(&path))
            .header("Authorization", self.auth.as_str())
            .header("User-Agent", self.user_agent.as_str())
            .call()
            .map_err(|e| anyhow!("Jellyfin request failed: {e}"))?;
        ok(response, &path).map(drop)
    }

    fn upload(&self, item: &str, kind: Kind, data: &[u8]) -> Result<()> {
        let path = image_path(item, kind, None);
        let body = base64::engine::general_purpose::STANDARD.encode(data);
        let response = self
            .agent
            .post(self.url(&path))
            .header("Authorization", self.auth.as_str())
            .header("User-Agent", self.user_agent.as_str())
            .header("Content-Type", sniff(data).0)
            .send(body)
            .map_err(|e| anyhow!("Jellyfin request failed: {e}"))?;
        ok(response, &path).map(drop)
    }

    fn refresh(&self, item: &str) -> Result<()> {
        let path = format!(
            "/Items/{item}/Refresh?metadataRefreshMode=ValidationOnly&imageRefreshMode=ValidationOnly&replaceAllMetadata=false&replaceAllImages=false"
        );
        let response = self
            .agent
            .post(self.url(&path))
            .header("Authorization", self.auth.as_str())
            .header("User-Agent", self.user_agent.as_str())
            .send_empty()
            .map_err(|e| anyhow!("Jellyfin request failed: {e}"))?;
        ok(response, &path).map(drop)
    }
}

/// MIME type and extension, from the image's first bytes.
fn sniff(data: &[u8]) -> (&'static str, &'static str) {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        ("image/jpeg", "jpg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", "png")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        ("image/webp", "webp")
    } else if data.len() >= 12
        && &data[4..8] == b"ftyp"
        && matches!(&data[8..12], b"avif" | b"avis")
    {
        ("image/avif", "avif")
    } else if data.starts_with(b"GIF8") {
        ("image/gif", "gif")
    } else {
        ("application/octet-stream", "bin")
    }
}

/// A folder name as Jellyfin and macOS would agree on it: composed (NFC), with
/// the private-use characters a Samba server stores for `" * : < > ? \ |`
/// mapped back, and `/` read as `:` (how macOS shows it).
pub fn normalize(name: &str) -> String {
    name.nfc()
        .map(|c| match c {
            '\u{F020}' => '"',
            '\u{F021}' => '*',
            '\u{F022}' | '/' => ':',
            '\u{F023}' => '<',
            '\u{F024}' => '>',
            '\u{F025}' => '?',
            '\u{F026}' => '\\',
            '\u{F027}' => '|',
            '\u{F028}' => ' ',
            '\u{F029}' => '.',
            c => c,
        })
        .collect()
}

/// Jellyfin's album artists that live in a music library folder, to look up by
/// local folder.
pub struct Index {
    artists: Vec<Artist>,
}

impl Index {
    pub fn new(artists: Vec<Artist>, locations: &[String]) -> Self {
        let inside = |path: &str| {
            locations
                .iter()
                .any(|l| path.starts_with(&format!("{}/", l.trim_end_matches('/'))))
        };
        let artists = artists
            .into_iter()
            .filter(|a| a.path.as_deref().is_some_and(inside))
            .collect();
        Self { artists }
    }

    /// The artist for a local artist folder: the one with this `MusicBrainz` id
    /// and folder name, else the only one with that folder name.
    pub fn find(&self, folder: &Path, mbid: Option<&str>) -> Option<&Artist> {
        let name = normalize(&folder.file_name()?.to_string_lossy());
        let named: Vec<&Artist> = self
            .artists
            .iter()
            .filter(|a| a.folder_name().is_some_and(|n| normalize(n) == name))
            .collect();
        if let Some(mbid) = mbid
            && let Some(artist) = named.iter().find(|a| a.mbid() == Some(mbid))
        {
            return Some(artist);
        }
        match named.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// How [`sync`] runs.
#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub dry_run: bool,
    /// Where images are saved before Jellyfin deletes them.
    pub backups: &'a Path,
    /// Unix seconds, in backup names.
    pub stamp: u64,
    /// The music library saves artwork into the media folders, so an upload
    /// would land in the artist folder: backdrops aren't reordered then.
    pub saves_locally: bool,
    /// How long to wait between looks while Jellyfin rescans, and how many.
    pub poll: Duration,
    pub polls: u32,
}

/// What [`sync`] did (or, on a dry run, would do) for an artist.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Synced {
    pub rescanned: bool,
    /// The folder files that are now Jellyfin's Primary or Logo.
    pub replaced: Vec<String>,
    /// Backdrops that were ahead of `backdrop.*` and now come after it.
    pub moved_behind: usize,
    pub notes: Vec<String>,
}

impl Synced {
    pub fn changed(&self) -> bool {
        self.rescanned || !self.replaced.is_empty() || self.moved_behind > 0
    }

    /// One line on what changed, or `None` when nothing did.
    pub fn describe(&self, dry_run: bool) -> Option<String> {
        let mut parts = Vec::new();
        if !self.replaced.is_empty() {
            parts.push(format!("shows {}", self.replaced.join(", ")));
        }
        if self.moved_behind > 0 {
            parts.push(format!(
                "backdrop first ({} older backdrop(s) moved after it)",
                self.moved_behind
            ));
        }
        if parts.is_empty() && self.rescanned {
            parts.push("rescanned".to_string());
        }
        if parts.is_empty() {
            return None;
        }
        let verb = if dry_run {
            "would make Jellyfin"
        } else {
            "Jellyfin now"
        };
        Some(format!("{verb}: {}", parts.join("; ")))
    }
}

/// The folder's own `folder.*`, `logo.*` and `backdrop.*` (no sidecars).
fn local_files(folder: &Path) -> Result<Vec<(Kind, String)>> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .with_context(|| format!("reading {}", folder.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut found = Vec::new();
    for kind in Kind::ALL {
        let name = names.iter().find(|name| {
            name.split_once('.').is_some_and(|(stem, ext)| {
                // `folder.jpg.sha256` and other sidecars have a second dot.
                stem == kind.stem() && !ext.contains('.')
            })
        });
        if let Some(name) = name {
            found.push((kind, name.clone()));
        }
    }
    Ok(found)
}

/// A library's subfolders (not hidden ones), sorted.
pub fn subfolders(library: &Path) -> Result<Vec<PathBuf>> {
    let mut folders: Vec<PathBuf> = fs::read_dir(library)
        .with_context(|| format!("reading {}", library.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    folders.sort();
    Ok(folders)
}

/// The subfolders of a library that hold artist images.
pub fn artist_folders(library: &Path) -> Result<Vec<PathBuf>> {
    Ok(subfolders(library)?
        .into_iter()
        .filter(|path| local_files(path).is_ok_and(|files| !files.is_empty()))
        .collect())
}

/// The image is the artist folder's `name`.
fn is_file(base: &str, image: &ImageInfo, name: &str) -> bool {
    image
        .path
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|rest| normalize(rest) == normalize(name))
}

/// The image lives in the artist folder (so it's never deleted).
fn in_folder(base: &str, image: &ImageInfo) -> bool {
    image
        .path
        .strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn of_kind(images: &[ImageInfo], kind: Kind) -> Vec<&ImageInfo> {
    let mut found: Vec<&ImageInfo> = images.iter().filter(|i| i.kind() == Some(kind)).collect();
    found.sort_by_key(|i| i.image_index.unwrap_or(0));
    found
}

/// Look at the images until `done` holds, or the polls run out.
fn wait_for(
    api: &impl Api,
    item: &str,
    opts: &Options<'_>,
    done: impl Fn(&[ImageInfo]) -> bool,
) -> Result<Vec<ImageInfo>> {
    let mut images = api.images(item)?;
    for _ in 0..opts.polls {
        if done(&images) {
            break;
        }
        sleep(opts.poll);
        images = api.images(item)?;
    }
    Ok(images)
}

fn back_up(
    opts: &Options<'_>,
    artist: &Artist,
    kind: Kind,
    index: Option<u32>,
    data: &[u8],
) -> Result<PathBuf> {
    let dir = opts.backups.join(&artist.id);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let index = index.map_or_else(String::new, |i| format!("-{i}"));
    let path = dir.join(format!(
        "{}{index}-{}.{}",
        kind.stem(),
        opts.stamp,
        sniff(data).1
    ));
    fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Make the artist folder's images the ones Jellyfin shows for `artist`.
pub fn sync(api: &impl Api, artist: &Artist, folder: &Path, opts: &Options<'_>) -> Result<Synced> {
    let base = artist
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("Jellyfin has no folder for {}", artist.name))?;
    let local = local_files(folder)?;
    let mut synced = Synced::default();
    if local.is_empty() {
        return Ok(synced);
    }
    let missing = |images: &[ImageInfo]| -> Vec<&(Kind, String)> {
        local
            .iter()
            .filter(|(kind, name)| {
                !of_kind(images, *kind)
                    .iter()
                    .any(|image| is_file(base, image, name))
            })
            .collect()
    };

    let mut images = api.images(&artist.id)?;
    if !missing(&images).is_empty() {
        synced.rescanned = true;
        if !opts.dry_run {
            api.refresh(&artist.id)?;
            images = wait_for(api, &artist.id, opts, |images| missing(images).is_empty())?;
        }
    }

    for (kind, name) in local.iter().filter(|(kind, _)| *kind != Kind::Backdrop) {
        images = show_single(api, artist, base, (*kind, name), images, opts, &mut synced)?;
    }
    if let Some((_, name)) = local.iter().find(|(kind, _)| *kind == Kind::Backdrop) {
        backdrop_first(api, artist, base, name, &images, opts, &mut synced)?;
    }
    Ok(synced)
}

/// Primary or Logo: Jellyfin holds one, and shows the folder's file once its
/// own copy is gone.
fn show_single(
    api: &impl Api,
    artist: &Artist,
    base: &str,
    (kind, name): (Kind, &str),
    images: Vec<ImageInfo>,
    opts: &Options<'_>,
    synced: &mut Synced,
) -> Result<Vec<ImageInfo>> {
    let shown = of_kind(&images, kind).first().map(|&image| image.clone());
    let Some(shown) = shown else {
        if !opts.dry_run {
            synced
                .notes
                .push(format!("Jellyfin hasn't picked up {name} yet"));
        }
        return Ok(images);
    };
    if is_file(base, &shown, name) {
        return Ok(images);
    }
    if in_folder(base, &shown) {
        synced.notes.push(format!(
            "Jellyfin shows {} instead of {name}; left alone",
            shown.path
        ));
        return Ok(images);
    }
    synced.replaced.push(name.to_string());
    if opts.dry_run {
        return Ok(images);
    }
    let data = api.download(&artist.id, kind, shown.image_index)?;
    back_up(opts, artist, kind, shown.image_index, &data)?;
    api.delete(&artist.id, kind, shown.image_index)?;
    api.refresh(&artist.id)?;
    let images = wait_for(api, &artist.id, opts, |images| {
        of_kind(images, kind)
            .first()
            .is_some_and(|image| is_file(base, image, name))
    })?;
    if !of_kind(&images, kind)
        .first()
        .is_some_and(|image| is_file(base, image, name))
    {
        synced
            .notes
            .push(format!("Jellyfin hasn't picked up {name} yet"));
    }
    Ok(images)
}

/// Put the folder's backdrop first: the backdrops ahead of it are deleted and
/// uploaded again, which appends them after it.
fn backdrop_first(
    api: &impl Api,
    artist: &Artist,
    base: &str,
    name: &str,
    images: &[ImageInfo],
    opts: &Options<'_>,
    synced: &mut Synced,
) -> Result<()> {
    let backdrops = of_kind(images, Kind::Backdrop);
    let Some(at) = backdrops
        .iter()
        .position(|image| is_file(base, image, name))
    else {
        if !opts.dry_run {
            synced
                .notes
                .push(format!("Jellyfin hasn't picked up {name} yet"));
        }
        return Ok(());
    };
    let ahead = &backdrops[..at];
    if ahead.is_empty() {
        return Ok(());
    }
    if let Some(own) = ahead.iter().find(|image| in_folder(base, image)) {
        synced.notes.push(format!(
            "{} comes before {name} in Jellyfin and is in the artist folder; left alone",
            own.path
        ));
        return Ok(());
    }
    if opts.saves_locally {
        synced.notes.push(format!(
            "{name} isn't first in Jellyfin, but the library saves artwork into media folders; left alone"
        ));
        return Ok(());
    }
    synced.moved_behind = ahead.len();
    if opts.dry_run {
        return Ok(());
    }

    let mut saved = Vec::new();
    for image in ahead {
        let data = api.download(&artist.id, Kind::Backdrop, image.image_index)?;
        let backup = back_up(opts, artist, Kind::Backdrop, image.image_index, &data)?;
        saved.push((data, backup));
    }
    for image in ahead.iter().rev() {
        api.delete(&artist.id, Kind::Backdrop, image.image_index)?;
    }
    for (data, backup) in &saved {
        api.upload(&artist.id, Kind::Backdrop, data)
            .with_context(|| {
                format!(
                    "uploading an older backdrop again (saved at {})",
                    backup.display()
                )
            })?;
    }
    let images = api.images(&artist.id)?;
    if !of_kind(&images, Kind::Backdrop)
        .first()
        .is_some_and(|image| is_file(base, image, name))
    {
        bail!("{name} still isn't Jellyfin's first backdrop");
    }
    Ok(())
}

/// What [`pull`] did (or, on a dry run, would do) for an artist.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pulled {
    /// Files written into the artist folder.
    pub wrote: Vec<String>,
    /// Images saved into `.gatefold-archive/`, as the folder had that file.
    pub archived: usize,
    /// Images the folder or its archive already held.
    pub already: usize,
    /// Images that aren't JPEG or PNG, left in Jellyfin.
    pub unsupported: usize,
}

impl Pulled {
    /// One line on what was brought in, or `None` when nothing was.
    pub fn describe(&self, dry_run: bool) -> Option<String> {
        let mut parts = Vec::new();
        if !self.wrote.is_empty() {
            let verb = if dry_run { "would write" } else { "wrote" };
            parts.push(format!("{verb} {}", self.wrote.join(", ")));
        }
        if self.archived > 0 {
            let verb = if dry_run { "would archive" } else { "archived" };
            parts.push(format!("{verb} {}", self.archived));
        }
        if self.unsupported > 0 {
            parts.push(format!(
                "{} not JPEG/PNG, left in Jellyfin",
                self.unsupported
            ));
        }
        (!parts.is_empty()).then(|| format!("pulled from Jellyfin: {}", parts.join("; ")))
    }
}

/// Copy the images only Jellyfin has for `artist` into `folder`: each is
/// written when the folder has no file of its kind (and no skip marker turns
/// it away), otherwise archived, dated, for `gatefold restore`. Images in the
/// artist folder are the folder's own and aren't pulled. Jellyfin isn't changed.
pub fn pull(
    api: &impl Api,
    artist: &Artist,
    folder: &Path,
    revisions: &Revisions,
    opts: &Options<'_>,
) -> Result<Pulled> {
    let base = artist
        .path
        .as_deref()
        .ok_or_else(|| anyhow!("Jellyfin has no folder for {}", artist.name))?;
    let images = api.images(&artist.id)?;
    let mut pulled = Pulled::default();
    for kind in Kind::ALL {
        let file = kind.artist_image();
        let theirs = of_kind(&images, kind)
            .into_iter()
            .filter(|image| !in_folder(base, image));
        for image in theirs {
            if opts.dry_run {
                let name = format!("{}.*", kind.stem());
                if file.existing(folder).is_none() && !pulled.wrote.contains(&name) {
                    pulled.wrote.push(name);
                } else {
                    pulled.archived += 1;
                }
                continue;
            }
            let original = api.download(&artist.id, kind, image.image_index)?;
            let ext @ ("jpg" | "png") = sniff(&original).1 else {
                pulled.unsupported += 1;
                continue;
            };
            let data = lossless::image(&original).unwrap_or_else(|| original.clone());
            let rejected = skip::rejects(folder, &data) || skip::rejects(folder, &original);
            let existing = file.existing(folder);
            // What an earlier pull wrote is already here: not archived again.
            if existing
                .as_deref()
                .is_some_and(|path| fs::read(path).is_ok_and(|current| current == data))
            {
                pulled.already += 1;
            } else if existing.is_none() && !rejected {
                let name = format!("{}.{ext}", kind.stem());
                write_new(folder, &name, &data)?;
                pulled.wrote.push(name);
            } else if revisions
                .archive_bytes(&file.archive_dir(folder), kind.stem(), ext, &data)?
                .is_some()
            {
                pulled.archived += 1;
            } else {
                pulled.already += 1;
            }
        }
    }
    Ok(pulled)
}

/// Write a new artist file (aside first, so an interrupted run never leaves a
/// partial image) with its digest sidecar.
fn write_new(folder: &Path, name: &str, data: &[u8]) -> Result<()> {
    let target = folder.join(name);
    let work = folder.join(format!(".{name}.part"));
    fs::write(&work, data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, &target)
        .with_context(|| format!("moving {} to {}", work.display(), target.display()))?;
    revision::write_digest(&target, &digest_bytes(data))
}

/// A connected server: the client, its artists, and the library settings.
pub struct Session {
    client: Client,
    index: Index,
    saves_locally: bool,
}

impl Session {
    pub fn connect(url: &str, api_key: &str, user_agent: String) -> Result<Self> {
        let client = Client::new(url, api_key, user_agent);
        let (locations, saves_locally) = client.music_libraries()?;
        let index = Index::new(client.artists()?, &locations);
        Ok(Self {
            client,
            index,
            saves_locally,
        })
    }

    pub fn find(&self, folder: &Path, mbid: Option<&str>) -> Option<&Artist> {
        self.index.find(folder, mbid)
    }

    /// Sync one artist folder; `None` when Jellyfin has no artist for it.
    pub fn sync_folder(
        &self,
        folder: &Path,
        mbid: Option<&str>,
        opts: &Options<'_>,
    ) -> Option<Result<Synced>> {
        let artist = self.find(folder, mbid)?;
        let opts = Options {
            saves_locally: self.saves_locally,
            ..*opts
        };
        Some(sync(&self.client, artist, folder, &opts))
    }

    /// Pull one artist folder's images; `None` when Jellyfin has no artist for it.
    pub fn pull_folder(
        &self,
        folder: &Path,
        mbid: Option<&str>,
        revisions: &Revisions,
        opts: &Options<'_>,
    ) -> Option<Result<Pulled>> {
        let artist = self.find(folder, mbid)?;
        Some(pull(&self.client, artist, folder, revisions, opts))
    }
}

/// Counts for a pull's summary.
#[derive(Debug, Default)]
pub struct PullTally {
    pub wrote: u64,
    pub archived: u64,
    pub already: u64,
    pub unsupported: u64,
    pub errors: u64,
}

impl PullTally {
    /// Count one folder and return its log line, if it needs one.
    pub fn record(&mut self, label: &str, result: Result<Pulled>, dry_run: bool) -> Option<String> {
        match result {
            Err(err) => {
                self.errors += 1;
                Some(format!("✗ {label}: Jellyfin: {err:#}"))
            }
            Ok(pulled) => {
                self.wrote += pulled.wrote.len() as u64;
                self.archived += pulled.archived as u64;
                self.already += pulled.already as u64;
                self.unsupported += pulled.unsupported as u64;
                pulled
                    .describe(dry_run)
                    .map(|line| format!("✓ {label}: {line}"))
            }
        }
    }

    pub fn summary(&self, dry_run: bool) -> String {
        let (wrote, archived) = if dry_run {
            ("would write", "would archive")
        } else {
            ("wrote", "archived")
        };
        format!(
            "{wrote} {} file(s), {archived} {}, already archived {}, not JPEG/PNG {}, errors {}",
            self.wrote, self.archived, self.already, self.unsupported, self.errors
        )
    }
}

/// Counts for a run's summary.
#[derive(Debug, Default)]
pub struct Tally {
    pub updated: u64,
    pub in_order: u64,
    pub not_found: u64,
    pub errors: u64,
}

impl Tally {
    /// Count one folder and return its log line, if it needs one.
    pub fn record(
        &mut self,
        label: &str,
        result: Option<Result<Synced>>,
        dry_run: bool,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        match result {
            None => {
                self.not_found += 1;
                lines.push(format!("· {label}: not in Jellyfin"));
            }
            Some(Err(err)) => {
                self.errors += 1;
                lines.push(format!("✗ {label}: Jellyfin: {err:#}"));
            }
            Some(Ok(synced)) => {
                if synced.changed() {
                    self.updated += 1;
                } else {
                    self.in_order += 1;
                }
                if let Some(line) = synced.describe(dry_run) {
                    lines.push(format!("✓ {label}: {line}"));
                }
                lines.extend(synced.notes.iter().map(|note| format!("· {label}: {note}")));
            }
        }
        lines
    }

    pub fn summary(&self, dry_run: bool) -> String {
        let updated = if dry_run { "would update" } else { "updated" };
        format!(
            "{updated} {}, already in order {}, not in Jellyfin {}, errors {}",
            self.updated, self.in_order, self.not_found, self.errors
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const BASE: &str = "/media/music/AFI";
    const META: &str = "/config/metadata/library/74/742607";

    /// A Jellyfin stand-in: images per kind in order, the files it would find
    /// in the artist folder on a refresh, and every call that writes.
    #[derive(Default)]
    struct Fake {
        images: RefCell<Vec<(Kind, String, Vec<u8>)>>,
        disk: Vec<(Kind, String, Vec<u8>)>,
        writes: RefCell<Vec<String>>,
        uploads: RefCell<u32>,
    }

    impl Fake {
        fn with(images: &[(Kind, &str, &[u8])], disk: &[(Kind, &str, &[u8])]) -> Self {
            let own = |list: &[(Kind, &str, &[u8])]| {
                list.iter()
                    .map(|(k, p, d)| (*k, (*p).to_string(), d.to_vec()))
                    .collect::<Vec<_>>()
            };
            Self {
                images: RefCell::new(own(images)),
                disk: own(disk),
                ..Self::default()
            }
        }

        fn position(&self, kind: Kind, index: Option<u32>) -> usize {
            let index = index.unwrap_or(0) as usize;
            self.images
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (k, ..))| *k == kind)
                .nth(index)
                .map(|(i, _)| i)
                .expect("no such image")
        }

        fn paths(&self, kind: Kind) -> Vec<String> {
            self.images
                .borrow()
                .iter()
                .filter(|(k, ..)| *k == kind)
                .map(|(_, p, _)| p.clone())
                .collect()
        }

        fn data(&self, kind: Kind) -> Vec<Vec<u8>> {
            self.images
                .borrow()
                .iter()
                .filter(|(k, ..)| *k == kind)
                .map(|(.., d)| d.clone())
                .collect()
        }
    }

    impl Api for Fake {
        fn images(&self, _: &str) -> Result<Vec<ImageInfo>> {
            let mut seen: HashMap<&str, u32> = HashMap::new();
            Ok(self
                .images
                .borrow()
                .iter()
                .map(|(kind, path, _)| {
                    let n = seen.entry(kind.api_name()).or_default();
                    let index = (*kind == Kind::Backdrop).then_some(*n);
                    *n += 1;
                    ImageInfo {
                        image_type: kind.api_name().to_string(),
                        image_index: index,
                        path: path.clone(),
                    }
                })
                .collect())
        }

        fn download(&self, _: &str, kind: Kind, index: Option<u32>) -> Result<Vec<u8>> {
            let at = self.position(kind, index);
            Ok(self.images.borrow()[at].2.clone())
        }

        fn delete(&self, _: &str, kind: Kind, index: Option<u32>) -> Result<()> {
            let at = self.position(kind, index);
            self.writes
                .borrow_mut()
                .push(format!("delete {kind:?} {index:?}"));
            self.images.borrow_mut().remove(at);
            Ok(())
        }

        fn upload(&self, _: &str, kind: Kind, data: &[u8]) -> Result<()> {
            self.writes.borrow_mut().push(format!("upload {kind:?}"));
            let mut n = self.uploads.borrow_mut();
            *n += 1;
            self.images
                .borrow_mut()
                .push((kind, format!("{META}/upload{n}.jpg"), data.to_vec()));
            Ok(())
        }

        /// Files in the folder are added: a backdrop at the end, a Primary or
        /// Logo only when Jellyfin has none of its own.
        fn refresh(&self, _: &str) -> Result<()> {
            self.writes.borrow_mut().push("refresh".to_string());
            let mut images = self.images.borrow_mut();
            for (kind, path, data) in &self.disk {
                let known = images.iter().any(|(_, p, _)| p == path);
                let single_taken =
                    *kind != Kind::Backdrop && images.iter().any(|(k, ..)| k == kind);
                if !known && !single_taken {
                    images.push((*kind, path.clone(), data.clone()));
                }
            }
            Ok(())
        }
    }

    fn artist() -> Artist {
        Artist {
            id: "742607".into(),
            name: "AFI".into(),
            path: Some(BASE.into()),
            provider_ids: HashMap::new(),
        }
    }

    fn folder(files: &[&str]) -> tempfile_dir::Dir {
        let dir = tempfile_dir::Dir::new();
        for file in files {
            fs::write(dir.path().join(file), b"x").unwrap();
        }
        dir
    }

    fn opts(backups: &Path, dry_run: bool) -> Options<'_> {
        Options {
            dry_run,
            backups,
            stamp: 1,
            saves_locally: false,
            poll: Duration::ZERO,
            polls: 2,
        }
    }

    fn local(name: &str) -> String {
        format!("{BASE}/{name}")
    }

    fn meta(name: &str) -> String {
        format!("{META}/{name}")
    }

    #[test]
    fn moves_older_backdrops_after_the_folders_one() {
        let dir = folder(&["backdrop.jpg", "backdrop.jpg.sha256"]);
        let backups = tempfile_dir::Dir::new();
        let fake = Fake::with(
            &[
                (Kind::Backdrop, &meta("backdrop.jpg"), b"old"),
                (Kind::Backdrop, &meta("backdrop1.webp"), b"older"),
                (Kind::Backdrop, &local("backdrop.jpg"), b"ours"),
            ],
            &[],
        );
        let synced = sync(&fake, &artist(), dir.path(), &opts(backups.path(), false)).unwrap();
        assert_eq!(synced.moved_behind, 2);
        assert_eq!(fake.paths(Kind::Backdrop)[0], local("backdrop.jpg"));
        assert_eq!(
            fake.data(Kind::Backdrop),
            vec![b"ours".to_vec(), b"old".to_vec(), b"older".to_vec()]
        );
        let backed_up = fs::read_dir(backups.path().join("742607")).unwrap().count();
        assert_eq!(backed_up, 2);
    }

    #[test]
    fn leaves_backdrops_already_in_order() {
        let dir = folder(&["backdrop.jpg"]);
        let fake = Fake::with(
            &[
                (Kind::Backdrop, &local("backdrop.jpg"), b"ours"),
                (Kind::Backdrop, &meta("backdrop.jpg"), b"old"),
            ],
            &[],
        );
        let synced = sync(&fake, &artist(), dir.path(), &opts(dir.path(), false)).unwrap();
        assert!(!synced.changed());
        assert_eq!(fake.writes.borrow().len(), 0);
    }

    #[test]
    fn never_deletes_images_in_the_artist_folder() {
        let dir = folder(&["backdrop.jpg", "logo.png"]);
        let fake = Fake::with(
            &[
                (Kind::Logo, &local("logo-old.png"), b"theirs"),
                (Kind::Backdrop, &local("backdrop1.jpg"), b"theirs"),
                (Kind::Backdrop, &local("backdrop.jpg"), b"ours"),
            ],
            &[(Kind::Logo, &local("logo.png"), b"ours")],
        );
        let synced = sync(&fake, &artist(), dir.path(), &opts(dir.path(), false)).unwrap();
        assert_eq!(synced.moved_behind, 0);
        assert_eq!(synced.replaced, Vec::<String>::new());
        assert_eq!(synced.notes.len(), 2, "{:?}", synced.notes);
        assert!(!fake.writes.borrow().iter().any(|w| w.starts_with("delete")));
    }

    #[test]
    fn leaves_backdrops_alone_when_uploads_would_land_in_the_folder() {
        let dir = folder(&["backdrop.jpg"]);
        let fake = Fake::with(
            &[
                (Kind::Backdrop, &meta("backdrop.jpg"), b"old"),
                (Kind::Backdrop, &local("backdrop.jpg"), b"ours"),
            ],
            &[],
        );
        let opts = Options {
            saves_locally: true,
            ..opts(dir.path(), false)
        };
        let synced = sync(&fake, &artist(), dir.path(), &opts).unwrap();
        assert_eq!(synced.moved_behind, 0);
        assert_eq!(synced.notes.len(), 1);
        assert_eq!(fake.writes.borrow().len(), 0);
    }

    #[test]
    fn swaps_jellyfins_own_photo_for_the_folders() {
        let dir = folder(&["folder.jpg"]);
        let backups = tempfile_dir::Dir::new();
        let fake = Fake::with(
            &[(Kind::Primary, &meta("folder.jpg"), b"old")],
            &[(Kind::Primary, &local("folder.jpg"), b"ours")],
        );
        let synced = sync(&fake, &artist(), dir.path(), &opts(backups.path(), false)).unwrap();
        assert_eq!(synced.replaced, vec!["folder.jpg".to_string()]);
        assert_eq!(fake.paths(Kind::Primary), vec![local("folder.jpg")]);
        let backup = backups.path().join("742607/folder-1.bin");
        assert_eq!(fs::read(backup).unwrap(), b"old");
    }

    #[test]
    fn rescans_when_jellyfin_misses_a_file() {
        let dir = folder(&["logo.png"]);
        let fake = Fake::with(&[], &[(Kind::Logo, &local("logo.png"), b"ours")]);
        let synced = sync(&fake, &artist(), dir.path(), &opts(dir.path(), false)).unwrap();
        assert!(synced.rescanned);
        assert_eq!(synced.notes, Vec::<String>::new());
        assert_eq!(fake.paths(Kind::Logo), vec![local("logo.png")]);
    }

    #[test]
    fn dry_runs_write_nothing() {
        let dir = folder(&["folder.jpg", "backdrop.jpg"]);
        let fake = Fake::with(
            &[
                (Kind::Primary, &meta("folder.jpg"), b"old"),
                (Kind::Backdrop, &meta("backdrop.jpg"), b"old"),
                (Kind::Backdrop, &local("backdrop.jpg"), b"ours"),
            ],
            &[(Kind::Primary, &local("folder.jpg"), b"ours")],
        );
        let synced = sync(&fake, &artist(), dir.path(), &opts(dir.path(), true)).unwrap();
        assert!(synced.rescanned);
        assert_eq!(synced.replaced, vec!["folder.jpg".to_string()]);
        assert_eq!(synced.moved_behind, 1);
        assert_eq!(fake.writes.borrow().len(), 0);
    }

    #[test]
    fn matches_folders_however_their_names_are_encoded() {
        let artist = |id: &str, path: &str, mbid: &str| Artist {
            id: id.into(),
            name: id.into(),
            path: Some(path.into()),
            provider_ids: HashMap::from([("MusicBrainzArtist".into(), mbid.into())]),
        };
        let index = Index::new(
            vec![
                artist("titas", "/media/music/Tit\u{e3}s", "t"),
                artist("badcop", "/media/music/Bad Cop\u{F022}Bad Cop", "b"),
                artist("a1", "/media/music/Twin", "one"),
                artist("a2", "/media/music/Twin", "two"),
                artist("meta", "/config/metadata/artists/Offspring", "o"),
            ],
            &["/media/music".to_string()],
        );
        let id = |name: &str, mbid: Option<&str>| {
            index.find(Path::new(name), mbid).map(|a| a.id.clone())
        };
        assert_eq!(id("/x/Tita\u{303}s", None).as_deref(), Some("titas"));
        assert_eq!(id("/x/Bad Cop:Bad Cop", None).as_deref(), Some("badcop"));
        assert_eq!(id("/x/Twin", Some("two")).as_deref(), Some("a2"));
        assert_eq!(id("/x/Twin", None), None);
        assert_eq!(id("/x/Offspring", Some("o")), None);
    }

    const JPEG_A: &[u8] = b"\xff\xd8\xff\xe0 older";
    const JPEG_B: &[u8] = b"\xff\xd8\xff\xe0 oldest";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n logo";
    const WEBP: &[u8] = b"RIFF\0\0\0\0WEBPVP8 ";

    fn archived(dir: &Path, sub: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join(".gatefold-archive").join(sub))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.ends_with(".sha256"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn pulls_missing_files_and_archives_the_rest() {
        let dir = folder(&["logo.png"]);
        let fake = Fake::with(
            &[
                (Kind::Logo, &meta("logo.png"), PNG),
                (Kind::Backdrop, &meta("backdrop.jpg"), JPEG_A),
                (Kind::Backdrop, &meta("backdrop1.jpg"), JPEG_B),
                (Kind::Backdrop, &meta("backdrop2.webp"), WEBP),
            ],
            &[],
        );
        let revisions = Revisions::new();
        let pulled = pull(
            &fake,
            &artist(),
            dir.path(),
            &revisions,
            &opts(dir.path(), false),
        )
        .unwrap();
        assert_eq!(pulled.wrote, vec!["backdrop.jpg".to_string()]);
        assert_eq!((pulled.archived, pulled.unsupported), (2, 1));
        assert_eq!(fs::read(dir.path().join("backdrop.jpg")).unwrap(), JPEG_A);
        assert!(dir.path().join("backdrop.jpg.sha256").is_file());
        assert_eq!(fs::read(dir.path().join("logo.png")).unwrap(), b"x");
        let date = revisions.date();
        assert_eq!(
            archived(dir.path(), "logos"),
            vec![format!("logo-{date}.png")]
        );
        assert_eq!(
            archived(dir.path(), "backdrops"),
            vec![format!("backdrop-{date}.jpg")]
        );
        assert_eq!(fake.writes.borrow().len(), 0, "Jellyfin is left alone");

        let again = pull(
            &fake,
            &artist(),
            dir.path(),
            &revisions,
            &opts(dir.path(), false),
        )
        .unwrap();
        assert_eq!(again.wrote, Vec::<String>::new());
        assert_eq!((again.archived, again.already), (0, 3));
    }

    #[test]
    fn never_pulls_the_folders_own_files() {
        let dir = folder(&[]);
        let fake = Fake::with(&[(Kind::Primary, &local("folder.jpg"), JPEG_A)], &[]);
        let pulled = pull(
            &fake,
            &artist(),
            dir.path(),
            &Revisions::new(),
            &opts(dir.path(), false),
        )
        .unwrap();
        assert_eq!(pulled, Pulled::default());
        assert!(!dir.path().join("folder.jpg").exists());
    }

    #[test]
    fn archives_what_a_skip_marker_turns_away() {
        let dir = folder(&[]);
        let marker = format!(".gatefold-{}-skip", digest_bytes(JPEG_A));
        fs::write(dir.path().join(marker), b"").unwrap();
        let fake = Fake::with(&[(Kind::Primary, &meta("folder.jpg"), JPEG_A)], &[]);
        let pulled = pull(
            &fake,
            &artist(),
            dir.path(),
            &Revisions::new(),
            &opts(dir.path(), false),
        )
        .unwrap();
        assert_eq!(pulled.archived, 1);
        assert!(!dir.path().join("folder.jpg").exists());
    }

    #[test]
    fn dry_pulls_write_nothing() {
        let dir = folder(&[]);
        let fake = Fake::with(
            &[
                (Kind::Backdrop, &meta("backdrop.jpg"), JPEG_A),
                (Kind::Backdrop, &meta("backdrop1.jpg"), JPEG_B),
            ],
            &[],
        );
        let pulled = pull(
            &fake,
            &artist(),
            dir.path(),
            &Revisions::new(),
            &opts(dir.path(), true),
        )
        .unwrap();
        assert_eq!(pulled.wrote, vec!["backdrop.*".to_string()]);
        assert_eq!(pulled.archived, 1);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn sniffs_image_formats() {
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]).1, "jpg");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").1, "webp");
        assert_eq!(sniff(b"\0\0\0\x1cftypavif").1, "avif");
    }

    /// A temporary directory removed on drop.
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "gatefold-jellyfin-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
