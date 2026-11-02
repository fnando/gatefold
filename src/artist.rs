//! Artist images (a square avatar and a wide backdrop) resolved from an artist's
//! public Apple Music page, falling back to Spotify for the avatar only.
//!
//! Neither service exposes these through a key-less API; the public pages are
//! fetched (identifying as gatefold) and the image URLs read out of their
//! `og:image` meta tag and (for Apple) the embedded `serialized-server-data`
//! JSON. Apple's image host renders any size and crop on request, which is how
//! both renditions come from a single source asset without an image crate.

use crate::apple::{asset_base, rendition};
use crate::musicbrainz::ArtistLinks;
use crate::net::{self, Image, Policy, RateLimitWaiter};
use crate::scanner;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use ureq::Agent;

const MAX_HTML_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const POLICY: Policy<'static> = Policy {
    service: "artist page",
    max_rate_limit_retries: Some(5),
    default_wait_secs: (10, 5),
    throttle_statuses: net::THROTTLE,
};
/// Apple's image host: `<w>x<h>bb` fits inside the box (capped at the source
/// size), `<w>x<h>sr` scales and centre-crops to exactly that aspect ratio.
pub const AVATAR_RENDITION: &str = "4000x4000bb.jpg";
pub const BACKDROP_RENDITION: &str = "3840x2160sr.jpg";
/// Artist photos live under this path; when an artist has none, Apple shows an
/// album cover (`…/Music…/cover.jpg/…`) instead, which we must not use.
const APPLE_ARTIST_ASSET_MARKER: &str = "AMCArtistImages";
const SPOTIFY_IMAGE_PREFIX: &str = "https://i.scdn.co/image/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Apple,
    Spotify,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Apple => "apple music",
            Source::Spotify => "spotify",
        }
    }
}

/// Image URLs resolved from an artist's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub source: Source,
    pub page_url: String,
    pub avatar_url: String,
    /// Spotify exposes no backdrop publicly.
    pub backdrop_url: Option<String>,
    /// Apple's 16:9 motion backdrop (an HLS master playlist), when the artist has one.
    pub video_url: Option<String>,
}

/// Bumped when a record gains fields a run needs to act on (v2: `video_url`), so
/// older records are re-resolved instead of silently lacking them.
pub const RECORD_VERSION: u32 = 2;

/// What was resolved for an artist and, if anything, what was last written —
/// cached as `artist/<mbid>.json`. The URLs double as provenance: a replace run
/// re-resolves and only rewrites when they differ.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRecord {
    #[serde(default)]
    pub version: u32,
    pub mbid: String,
    pub name: String,
    pub source: Source,
    pub page_url: String,
    pub avatar_url: String,
    #[serde(default)]
    pub backdrop_url: Option<String>,
    #[serde(default)]
    pub video_url: Option<String>,
    /// Unix seconds.
    pub resolved_at: u64,
    /// Unix seconds; `None` after a `--cache-only` resolution.
    #[serde(default)]
    pub written_at: Option<u64>,
}

impl ArtistRecord {
    pub fn new(mbid: &str, name: &str, resolved: &Resolved, now: u64) -> Self {
        Self {
            version: RECORD_VERSION,
            mbid: mbid.to_string(),
            name: name.to_string(),
            source: resolved.source,
            page_url: resolved.page_url.clone(),
            avatar_url: resolved.avatar_url.clone(),
            backdrop_url: resolved.backdrop_url.clone(),
            video_url: resolved.video_url.clone(),
            resolved_at: now,
            written_at: None,
        }
    }

    pub fn is_current(&self) -> bool {
        self.version == RECORD_VERSION
    }

    pub fn resolved(&self) -> Resolved {
        Resolved {
            source: self.source,
            page_url: self.page_url.clone(),
            avatar_url: self.avatar_url.clone(),
            backdrop_url: self.backdrop_url.clone(),
            video_url: self.video_url.clone(),
        }
    }

    /// True when `other` points at the same images this record was written from.
    pub fn same_images(&self, other: &Resolved) -> bool {
        self.avatar_url == other.avatar_url
            && self.backdrop_url == other.backdrop_url
            && self.video_url == other.video_url
    }
}

pub struct Resolver {
    agent: Agent,
    user_agent: String,
    delay: Duration,
}

impl Resolver {
    pub fn new(user_agent: String, delay: Duration) -> Self {
        Self {
            agent: net::agent(),
            user_agent,
            delay,
        }
    }

    /// Apple Music first (avatar + backdrop); if it has no artist photo, Spotify
    /// (avatar only). `None` when neither yields a usable image.
    pub fn resolve<W: RateLimitWaiter>(
        &self,
        links: &ArtistLinks,
        waiter: &W,
    ) -> Result<Option<Resolved>> {
        if let Some(id) = &links.apple_id
            && let Some(resolved) = self.apple(id, waiter)?
        {
            return Ok(Some(resolved));
        }
        if let Some(id) = &links.spotify_id
            && let Some(resolved) = self.spotify(id, waiter)?
        {
            return Ok(Some(resolved));
        }
        Ok(None)
    }

    fn apple<W: RateLimitWaiter>(&self, id: &str, waiter: &W) -> Result<Option<Resolved>> {
        let page_url = format!("https://music.apple.com/us/artist/{id}");
        let Some(html) = self.page(&page_url, "apple music", waiter)? else {
            return Ok(None);
        };
        Ok(apple_artwork(&html).map(|art| Resolved {
            source: Source::Apple,
            page_url,
            backdrop_url: Some(rendition(
                art.wide.as_deref().unwrap_or(&art.avatar),
                BACKDROP_RENDITION,
            )),
            avatar_url: rendition(&art.avatar, AVATAR_RENDITION),
            video_url: art.video,
        }))
    }

    fn spotify<W: RateLimitWaiter>(&self, id: &str, waiter: &W) -> Result<Option<Resolved>> {
        let page_url = format!("https://open.spotify.com/artist/{id}");
        let Some(html) = self.page(&page_url, "spotify", waiter)? else {
            return Ok(None);
        };
        Ok(og_image(&html)
            .filter(|url| url.starts_with(SPOTIFY_IMAGE_PREFIX))
            .map(|avatar_url| Resolved {
                source: Source::Spotify,
                page_url,
                avatar_url,
                backdrop_url: None,
                video_url: None,
            }))
    }

    /// Fetch an artist page. `None` on 404 (unknown artist). Requests identify as
    /// gatefold on purpose: Spotify serves the server-rendered page (with its
    /// `og:` tags) to non-browser agents, but a script-only shell without them
    /// to browser user agents.
    fn page<W: RateLimitWaiter>(
        &self,
        url: &str,
        service: &str,
        waiter: &W,
    ) -> Result<Option<String>> {
        let build = || {
            self.agent
                .get(url)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "text/html,application/xhtml+xml")
                .header("Accept-Language", "en-US,en;q=0.9")
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;
        match response.status().as_u16() {
            200 => Ok(Some(net::read_text(&mut response, MAX_HTML_BYTES)?)),
            404 => Ok(None),
            other => bail!("{service} returned {other} for {url}"),
        }
    }

    /// The `User-Agent` these requests identify with (also handed to ffmpeg).
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Fetch a small text resource such as an HLS playlist. `None` on 404.
    pub fn fetch_text<W: RateLimitWaiter>(&self, url: &str, waiter: &W) -> Result<Option<String>> {
        let build = || {
            self.agent
                .get(url)
                .header("User-Agent", self.user_agent.as_str())
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;
        match response.status().as_u16() {
            200 => Ok(Some(net::read_text(&mut response, 1024 * 1024)?)),
            404 | 403 => Ok(None),
            other => bail!("playlist host returned {other} for {url}"),
        }
    }

    /// Download one image rendition. `None` when it's gone or not an image.
    pub fn fetch_image<W: RateLimitWaiter>(&self, url: &str, waiter: &W) -> Result<Option<Image>> {
        let build = || {
            self.agent
                .get(url)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "image/jpeg,image/png,image/webp,image/*;q=0.8")
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;
        match response.status().as_u16() {
            200 => {
                let mime = net::content_type(&response, "image/jpeg");
                if !mime.starts_with("image/") {
                    return Ok(None);
                }
                let data = net::read_limited(&mut response, MAX_IMAGE_BYTES)?;
                if data.is_empty() {
                    return Ok(None);
                }
                let ext = net::extension_for(&mime);
                Ok(Some(Image { mime, ext, data }))
            }
            404 | 403 => Ok(None),
            other => bail!("image host returned {other} for {url}"),
        }
    }
}

// ---- Page parsing (pure) ------------------------------------------------------

/// What an Apple Music artist page offers for the artist's own visuals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleArtwork {
    /// Asset base of the artist photo.
    pub avatar: String,
    /// Asset base of a dedicated wide banner, when Apple has one.
    pub wide: Option<String>,
    /// HLS master playlist of the 16:9 motion backdrop, when the artist has one.
    pub video: Option<String>,
}

/// Apple's artwork for the artist. Prefers the page's embedded JSON (which
/// distinguishes the artist photo from the album-cover fallback shown for artists
/// without one, and is the only place the banner and video appear) and falls back
/// to `og:image`.
pub fn apple_artwork(html: &str) -> Option<AppleArtwork> {
    if let Some(json) = serialized_server_data(html)
        && let Ok(value) = serde_json::from_str::<Value>(json)
        && let Some(header) = object_with_key(&value, "circleArtwork")
        && let Some(avatar) = artwork_base(header.get("circleArtwork"))
    {
        let video = header
            .get("wideVideoArtwork")
            .and_then(|v| v.get("dictionary"))
            .and_then(|d| d.get("motionArtistFullscreen16x9"))
            .and_then(|m| m.get("video"))
            .and_then(Value::as_str)
            .filter(|url| url.starts_with("https://"))
            .map(str::to_string);
        return Some(AppleArtwork {
            avatar,
            wide: artwork_base(header.get("wideArtwork")),
            video,
        });
    }
    let base = asset_base(&og_image(html)?)?;
    is_artist_asset(&base).then_some(AppleArtwork {
        avatar: base,
        wide: None,
        video: None,
    })
}

/// The asset base of an Apple artwork object (`{ "dictionary": { "url": "…/{w}x{h}{c}.{f}" } }`),
/// or `None` when absent or not an artist photo.
fn artwork_base(artwork: Option<&Value>) -> Option<String> {
    let url = artwork?
        .get("dictionary")
        .and_then(|d| d.get("url"))
        .and_then(Value::as_str)?;
    let base = asset_base(url)?;
    is_artist_asset(&base).then_some(base)
}

fn is_artist_asset(base: &str) -> bool {
    base.contains(APPLE_ARTIST_ASSET_MARKER)
}

/// Depth-first search for the first object that has `key`.
fn object_with_key<'a>(value: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    match value {
        Value::Object(map) => {
            if map.contains_key(key) {
                return Some(map);
            }
            map.values().find_map(|v| object_with_key(v, key))
        }
        Value::Array(items) => items.iter().find_map(|v| object_with_key(v, key)),
        _ => None,
    }
}

/// The contents of `<script id="serialized-server-data">…</script>`.
pub fn serialized_server_data(html: &str) -> Option<&str> {
    let id = html.find("id=\"serialized-server-data\"")?;
    let open_end = id + html[id..].find('>')? + 1;
    let close = open_end + html[open_end..].find("</script>")?;
    Some(html[open_end..close].trim())
}

/// The `content` of `<meta property="og:image" …>`, with `&amp;` decoded.
pub fn og_image(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(start) = rest.find("<meta") {
        let tag_rest = &rest[start..];
        let end = tag_rest.find('>').map_or(tag_rest.len(), |i| i + 1);
        let tag = &tag_rest[..end];
        rest = &tag_rest[end..];
        let is_og_image = tag.contains("property=\"og:image\"")
            || tag.contains("property='og:image'")
            || tag.contains("name=\"og:image\"");
        if is_og_image && let Some(content) = attribute(tag, "content") {
            return Some(content.replace("&amp;", "&"));
        }
    }
    None
}

/// A quoted attribute value inside a single tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(i) = tag.find(&needle) {
            let value = &tag[i + needle.len()..];
            return value.find(quote).map(|end| &value[..end]);
        }
    }
    None
}

// ---- Artist folder ------------------------------------------------------------

/// Lowercased alphanumerics only, so `AC/DC` matches a folder named `AC-DC` or `ACDC`.
pub fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The folder artist images belong in: walking up from the track's folder (within
/// `root`), the nearest ancestor named after the album artist that holds no audio
/// files itself. Album folders are never used, even when self-titled, because
/// `folder.jpg` there would be read as the album cover (see `writer::COVER_FILENAMES`).
pub fn find_artist_folder(track_dir: &Path, root: &Path, album_artist: &str) -> Option<PathBuf> {
    let wanted = normalize_name(album_artist);
    if wanted.is_empty() {
        return None;
    }
    track_dir
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .find(|dir| {
            dir.file_name()
                .is_some_and(|n| normalize_name(&n.to_string_lossy()) == wanted)
                && !scanner::dir_has_audio(dir)
        })
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const APPLE_BASE: &str = "https://is1-ssl.mzstatic.com/image/thumb/AMCArtistImages211/v4/8a/49/84/x/ami-identity-abc_cropped.png";

    #[test]
    fn og_image_reads_the_meta_tag() {
        let html = r#"<html><head><meta property="og:title" content="X"/>
            <meta property="og:image:width" content="640"/>
            <meta property="og:image" content="https://i.scdn.co/image/abc?a=1&amp;b=2"/></head></html>"#;
        assert_eq!(
            og_image(html).as_deref(),
            Some("https://i.scdn.co/image/abc?a=1&b=2")
        );
        assert_eq!(og_image("<html></html>"), None);
    }

    #[test]
    fn apple_artwork_prefers_json_and_rejects_album_fallbacks() {
        let cover = "https://is1-ssl.mzstatic.com/image/thumb/Music124/v4/b7/cover.jpg";
        let json = format!(
            r#"{{"data":[{{"sections":[{{"items":[{{"artwork":{{}},"circleArtwork":{{"dictionary":{{"url":"{APPLE_BASE}/{{w}}x{{h}}{{c}}.{{f}}"}}}},"wideArtwork":null,"fallbackArtwork":{{"dictionary":{{"url":"{cover}/{{w}}x{{h}}{{c}}.{{f}}"}}}}}}]}}]}}]}}"#
        );
        let html = format!(
            r#"<meta property="og:image" content="{cover}/1200x630cw.png"><script type="application/json" id="serialized-server-data">{json}</script>"#
        );
        assert_eq!(
            apple_artwork(&html),
            Some(AppleArtwork {
                avatar: APPLE_BASE.to_string(),
                wide: None,
                video: None
            })
        );

        // No artist photo: JSON only carries the album-cover fallback, og:image too.
        let json = format!(
            r#"{{"items":[{{"circleArtwork":{{"dictionary":{{"url":"{cover}/{{w}}x{{h}}{{c}}.{{f}}"}}}}}}]}}"#
        );
        let html = format!(
            r#"<meta property="og:image" content="{cover}/1200x630cw.png"><script id="serialized-server-data">{json}</script>"#
        );
        assert_eq!(apple_artwork(&html), None);

        // No JSON: fall back to og:image.
        let html = format!(r#"<meta property="og:image" content="{APPLE_BASE}/1200x630cw.png">"#);
        assert_eq!(
            apple_artwork(&html).map(|a| a.avatar),
            Some(APPLE_BASE.to_string())
        );

        // A wide banner and a motion backdrop are picked up from the same header object.
        let wide = "https://is1-ssl.mzstatic.com/image/thumb/AMCArtistImages211/v4/00/wide.png";
        let video = "https://mvod.itunes.apple.com/itunes-assets/HLSMusic113/v4/x/P1_default.m3u8";
        let json = format!(
            r#"[{{"wideArtwork":{{"dictionary":{{"url":"{wide}/{{w}}x{{h}}{{c}}.{{f}}"}}}},"circleArtwork":{{"dictionary":{{"url":"{APPLE_BASE}/{{w}}x{{h}}{{c}}.{{f}}"}}}},"wideVideoArtwork":{{"dictionary":{{"motionArtistFullscreen16x9":{{"video":"{video}"}}}}}}}}]"#
        );
        let html = format!(r#"<script id="serialized-server-data">{json}</script>"#);
        assert_eq!(
            apple_artwork(&html),
            Some(AppleArtwork {
                avatar: APPLE_BASE.to_string(),
                wide: Some(wide.to_string()),
                video: Some(video.to_string())
            })
        );
    }

    #[test]
    #[ignore = "hits music.apple.com and open.spotify.com"]
    fn resolves_radiohead_live() {
        struct NoWait;
        impl RateLimitWaiter for NoWait {
            fn wait(&self, duration: Duration, _: &str) {
                std::thread::sleep(duration);
            }
        }
        let resolver = Resolver::new(
            "gatefold-test (https://github.com/fnando/gatefold)".into(),
            Duration::ZERO,
        );
        let apple = resolver.apple("657515", &NoWait).unwrap().unwrap();
        assert_eq!(apple.source, Source::Apple);
        assert!(
            apple.avatar_url.ends_with(AVATAR_RENDITION),
            "{}",
            apple.avatar_url
        );
        assert!(
            apple
                .backdrop_url
                .as_deref()
                .unwrap()
                .ends_with(BACKDROP_RENDITION)
        );
        let spotify = resolver
            .spotify("4Z8W4fKeB5YxbusRsdQVPb", &NoWait)
            .unwrap()
            .unwrap();
        assert_eq!(spotify.source, Source::Spotify);
        assert!(spotify.avatar_url.starts_with(SPOTIFY_IMAGE_PREFIX));
        assert_eq!(spotify.backdrop_url, None);
        let image = resolver
            .fetch_image(&spotify.avatar_url, &NoWait)
            .unwrap()
            .unwrap();
        assert!(image.mime.starts_with("image/") && !image.data.is_empty());
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_name("AC/DC"), "acdc");
        assert_eq!(normalize_name("Sigur Rós"), "sigurrós");
        assert_eq!(normalize_name("!!!"), "");
    }

    #[test]
    fn finds_the_artist_folder_above_album_folders() {
        let root = std::env::temp_dir().join(format!("gatefold-artist-{}", std::process::id()));
        let album = root.join("Radiohead").join("Radiohead");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("01.mp3"), b"").unwrap();
        let flat = root.join("Loose");
        fs::create_dir_all(&flat).unwrap();

        // Self-titled album folder holds audio, so the outer folder wins.
        assert_eq!(
            find_artist_folder(&album, &root, "radiohead"),
            Some(root.join("Radiohead"))
        );
        // The scan root itself can be the artist folder.
        assert_eq!(
            find_artist_folder(&album, &root.join("Radiohead"), "Radiohead"),
            Some(root.join("Radiohead"))
        );
        // Nothing above the track folder matches.
        assert_eq!(find_artist_folder(&flat, &root, "Loose Artist"), None);
        assert_eq!(find_artist_folder(&album, &root, "!!!"), None);

        fs::remove_dir_all(&root).unwrap();
    }
}
