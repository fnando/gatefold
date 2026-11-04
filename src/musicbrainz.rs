use crate::net::{self, Policy, RateLimitWaiter};
use anyhow::{Result, bail};
use serde_json::Value;
use std::time::Duration;
use ureq::Agent;

/// The special "Various Artists" artist; compilations carry it as album artist.
pub const VARIOUS_ARTISTS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";
/// The public server enforces one request per second per client.
const PUBLIC_HOST: &str = "musicbrainz.org";
const PUBLIC_MIN_DELAY: Duration = Duration::from_secs(1);
/// Artist lookups are a few KB; url-rels on prolific artists stay well under this.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// `MusicBrainz` signals throttling with 503 (and sometimes 429) plus Retry-After.
const POLICY: Policy<'static> = Policy {
    service: "musicbrainz",
    max_rate_limit_retries: Some(5),
    default_wait_secs: (5, 3),
    throttle_statuses: net::THROTTLE,
};

/// Streaming-service artist ids found among an artist's URL relations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistLinks {
    /// Numeric Apple Music artist id (`music.apple.com/<storefront>/artist/<id>`).
    pub apple_id: Option<String>,
    /// Spotify artist id (`open.spotify.com/artist/<id>`).
    pub spotify_id: Option<String>,
}

impl ArtistLinks {
    pub fn is_empty(&self) -> bool {
        self.apple_id.is_none() && self.spotify_id.is_none()
    }

    /// Take `other`'s ids for whichever services this has none for. True when
    /// anything was taken.
    pub fn fill_from(&mut self, other: ArtistLinks) -> bool {
        let mut filled = false;
        if self.apple_id.is_none() && other.apple_id.is_some() {
            self.apple_id = other.apple_id;
            filled = true;
        }
        if self.spotify_id.is_none() && other.spotify_id.is_some() {
            self.spotify_id = other.spotify_id;
            filled = true;
        }
        filled
    }
}

pub struct Client {
    agent: Agent,
    base_url: String,
    user_agent: String,
    delay: Duration,
}

impl Client {
    /// `base_url` is the web-service root (e.g. `https://musicbrainz.org` or a local
    /// mirror). The 1 req/s floor applies only when it points at the public server.
    pub fn new(base_url: &str, user_agent: String, delay: Duration) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let delay = if is_public(&base_url) {
            delay.max(PUBLIC_MIN_DELAY)
        } else {
            delay
        };
        Self {
            agent: net::agent(),
            base_url,
            user_agent,
            delay,
        }
    }

    /// True when requests go to the public `musicbrainz.org` service.
    pub fn is_public(&self) -> bool {
        is_public(&self.base_url)
    }

    /// The host requests are sent to, for the run headline.
    pub fn host(&self) -> String {
        host_of(&self.base_url).unwrap_or_else(|| self.base_url.clone())
    }

    /// The per-request delay in effect (after the public-server floor).
    pub fn delay(&self) -> Duration {
        self.delay
    }

    /// Fetch an artist with its URL relations. `None` when the id is unknown.
    pub fn artist<W: RateLimitWaiter>(&self, mbid: &str, waiter: &W) -> Result<Option<Value>> {
        self.get_json(&format!("artist/{mbid}"), &[("inc", "url-rels")], waiter)
    }

    /// Fetch a release (barcode included) with its URL relations.
    pub fn release<W: RateLimitWaiter>(&self, mbid: &str, waiter: &W) -> Result<Option<Value>> {
        self.get_json(&format!("release/{mbid}"), &[("inc", "url-rels")], waiter)
    }

    /// Browse every release in a release group, each with its URL relations, in
    /// one request (the first 100; groups rarely have more).
    pub fn releases_in_group<W: RateLimitWaiter>(
        &self,
        group: &str,
        waiter: &W,
    ) -> Result<Option<Value>> {
        self.get_json(
            "release",
            &[
                ("release-group", group),
                ("inc", "url-rels"),
                ("limit", "100"),
            ],
            waiter,
        )
    }

    fn get_json<W: RateLimitWaiter>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        waiter: &W,
    ) -> Result<Option<Value>> {
        let url = format!("{}/ws/2/{path}", self.base_url);
        let build = || {
            self.agent
                .get(&url)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "application/json")
                .query("fmt", "json")
                .query_pairs(params.iter().copied())
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;

        match response.status().as_u16() {
            200 => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES)?;
                let value: Value = serde_json::from_str(&body)?;
                Ok(Some(value))
            }
            404 => Ok(None),
            other => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES).unwrap_or_default();
                let body = body.trim();
                let detail = if body.len() > 200 { &body[..200] } else { body };
                bail!("musicbrainz returned {other}: {detail}")
            }
        }
    }
}

fn is_public(base_url: &str) -> bool {
    host_of(base_url).is_some_and(|h| h == PUBLIC_HOST || h.ends_with(".musicbrainz.org"))
}

/// The lowercased host of a URL (no scheme, credentials, port, or path).
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or(host_port));
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Pick the Apple Music / Spotify artist ids out of an artist lookup's `relations`.
/// The first current (not `ended`) link of each kind wins.
pub fn links_from(artist: &Value) -> ArtistLinks {
    let mut links = ArtistLinks::default();
    let Some(relations) = artist.get("relations").and_then(Value::as_array) else {
        return links;
    };
    for relation in relations {
        if relation.get("ended").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(url) = relation
            .get("url")
            .and_then(|u| u.get("resource"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if links.apple_id.is_none() {
            links.apple_id = apple_artist_id(url);
        }
        if links.spotify_id.is_none() {
            links.spotify_id = spotify_artist_id(url);
        }
    }
    links
}

/// The Wikidata item id (`Q44190`) among an artist's URL relations, if any.
pub fn wikidata_id(artist: &Value) -> Option<String> {
    artist
        .get("relations")?
        .as_array()?
        .iter()
        .filter(|r| r.get("ended").and_then(Value::as_bool) != Some(true))
        .filter_map(|r| r.get("url")?.get("resource")?.as_str())
        .find_map(wikidata_item_id)
}

/// `https://www.wikidata.org/wiki/Q44190` → `Q44190`.
pub fn wikidata_item_id(url: &str) -> Option<String> {
    let host = host_of(url)?;
    if host != "www.wikidata.org" && host != "wikidata.org" && host != "m.wikidata.org" {
        return None;
    }
    path_of(url)
        .split('/')
        .filter(|s| !s.is_empty())
        .find(|s| {
            s.strip_prefix('Q')
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// `https://music.apple.com/gb/artist/radiohead/657515` → `657515`. Also accepts
/// the older `itunes.apple.com/.../artist/id657515` form.
pub fn apple_artist_id(url: &str) -> Option<String> {
    apple_id(url, "artist")
}

/// `https://music.apple.com/gb/album/ok-computer/1097861387` → `1097861387`.
pub fn apple_album_id(url: &str) -> Option<String> {
    apple_id(url, "album")
}

/// The numeric id after the `kind` segment of an Apple Music / iTunes URL.
fn apple_id(url: &str, kind: &str) -> Option<String> {
    let host = host_of(url)?;
    if host != "music.apple.com" && host != "itunes.apple.com" && host != "geo.music.apple.com" {
        return None;
    }
    let path = path_of(url);
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    segments.find(|s| *s == kind)?;
    segments
        .map(|s| s.strip_prefix("id").unwrap_or(s))
        .rfind(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string)
}

/// The barcode recorded on a `MusicBrainz` release, if any.
pub fn release_barcode(release: &Value) -> Option<String> {
    release
        .get("barcode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
}

/// The Apple Music album id among a release's URL relations (first current one).
pub fn apple_album_link(release: &Value) -> Option<String> {
    release
        .get("relations")?
        .as_array()?
        .iter()
        .filter(|r| r.get("ended").and_then(Value::as_bool) != Some(true))
        .filter_map(|r| r.get("url")?.get("resource")?.as_str())
        .find_map(apple_album_id)
}

/// The first Apple Music album id among the releases of a browse response.
pub fn apple_album_in_group(browse: &Value) -> Option<String> {
    browse
        .get("releases")?
        .as_array()?
        .iter()
        .find_map(apple_album_link)
}

/// `https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb?si=…` → the id.
pub fn spotify_artist_id(url: &str) -> Option<String> {
    let host = host_of(url)?;
    if host != "open.spotify.com" && host != "play.spotify.com" {
        return None;
    }
    let path = path_of(url);
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    segments.find(|s| *s == "artist")?;
    segments
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_string)
}

/// The path of a URL, without query or fragment.
fn path_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    path.split(['?', '#']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_host_detection() {
        assert!(is_public("https://musicbrainz.org"));
        assert!(is_public("https://musicbrainz.org/"));
        assert!(is_public("https://beta.musicbrainz.org"));
        assert!(!is_public("http://localhost:5000"));
        assert!(!is_public("http://mb.lan/"));
        assert!(!is_public("http://notmusicbrainz.org"));
        assert_eq!(
            host_of("http://user:pw@[::1]:5000/x").as_deref(),
            Some("::1")
        );
    }

    #[test]
    fn public_floor_applies_only_to_the_public_server() {
        let public = Client::new("https://musicbrainz.org/", "ua".into(), Duration::ZERO);
        assert_eq!(public.delay(), Duration::from_secs(1));
        assert_eq!(public.host(), "musicbrainz.org");
        let local = Client::new("http://localhost:5000", "ua".into(), Duration::ZERO);
        assert_eq!(local.delay(), Duration::ZERO);
        assert!(!local.is_public());
    }

    #[test]
    fn extracts_ids_from_links() {
        assert_eq!(
            apple_artist_id("https://music.apple.com/gb/artist/radiohead/657515").as_deref(),
            Some("657515")
        );
        assert_eq!(
            apple_artist_id("https://music.apple.com/us/artist/657515?l=en").as_deref(),
            Some("657515")
        );
        assert_eq!(
            apple_artist_id("https://itunes.apple.com/us/artist/radiohead/id657515").as_deref(),
            Some("657515")
        );
        assert_eq!(
            apple_artist_id("https://music.apple.com/us/album/ok-computer/1097861387"),
            None
        );
        assert_eq!(
            apple_album_id("https://music.apple.com/us/album/ok-computer/1097861387").as_deref(),
            Some("1097861387")
        );
        assert_eq!(
            apple_album_id("https://music.apple.com/gb/artist/657515"),
            None
        );
        assert_eq!(
            apple_artist_id("https://open.spotify.com/artist/657515"),
            None
        );
        assert_eq!(
            spotify_artist_id("https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb?si=abc")
                .as_deref(),
            Some("4Z8W4fKeB5YxbusRsdQVPb")
        );
        assert_eq!(
            spotify_artist_id("https://open.spotify.com/album/abc"),
            None
        );
    }

    #[test]
    fn links_from_skips_ended_relations_and_keeps_the_first() {
        let artist = json!({
            "relations": [
                { "type": "streaming", "ended": true,
                  "url": { "resource": "https://open.spotify.com/artist/OLD" } },
                { "type": "free streaming",
                  "url": { "resource": "https://open.spotify.com/artist/NEW1" } },
                { "type": "free streaming",
                  "url": { "resource": "https://open.spotify.com/artist/NEW2" } },
                { "type": "wikidata", "url": { "resource": "https://www.wikidata.org/wiki/Q44190" } },
                { "type": "streaming",
                  "url": { "resource": "https://music.apple.com/gb/artist/657515" } }
            ]
        });
        let links = links_from(&artist);
        assert_eq!(links.spotify_id.as_deref(), Some("NEW1"));
        assert_eq!(links.apple_id.as_deref(), Some("657515"));
        assert!(links_from(&json!({})).is_empty());
        assert_eq!(wikidata_id(&artist).as_deref(), Some("Q44190"));
        assert_eq!(wikidata_id(&json!({})), None);
    }

    #[test]
    fn fills_missing_links_only() {
        let mut links = ArtistLinks {
            apple_id: None,
            spotify_id: Some("MINE".into()),
        };
        assert!(links.fill_from(ArtistLinks {
            apple_id: Some("1".into()),
            spotify_id: Some("THEIRS".into()),
        }));
        assert_eq!(links.apple_id.as_deref(), Some("1"));
        assert_eq!(links.spotify_id.as_deref(), Some("MINE"));
        assert!(!links.fill_from(ArtistLinks::default()));
    }

    #[test]
    fn parses_wikidata_urls() {
        assert_eq!(
            wikidata_item_id("https://www.wikidata.org/wiki/Q44190").as_deref(),
            Some("Q44190")
        );
        assert_eq!(
            wikidata_item_id("https://wikidata.org/entity/Q44190#sitelinks").as_deref(),
            Some("Q44190")
        );
        assert_eq!(wikidata_item_id("https://www.wikidata.org/wiki/P434"), None);
        assert_eq!(
            wikidata_item_id("https://en.wikipedia.org/wiki/Q44190"),
            None
        );
    }

    #[test]
    fn release_helpers() {
        let release = json!({
            "barcode": " 634904078164 ",
            "relations": [
                { "type": "streaming", "ended": true,
                  "url": { "resource": "https://music.apple.com/gb/album/1" } },
                { "type": "purchase for download",
                  "url": { "resource": "https://music.apple.com/gb/album/1097861387" } }
            ]
        });
        assert_eq!(release_barcode(&release).as_deref(), Some("634904078164"));
        assert_eq!(apple_album_link(&release).as_deref(), Some("1097861387"));
        assert_eq!(release_barcode(&json!({ "barcode": "" })), None);
        let browse = json!({ "releases": [ { "relations": [] }, release ] });
        assert_eq!(apple_album_in_group(&browse).as_deref(), Some("1097861387"));
        assert_eq!(apple_album_in_group(&json!({ "releases": [] })), None);
    }
}
