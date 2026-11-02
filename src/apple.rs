//! Apple Music album artwork: the public iTunes Lookup API (key-less, documented)
//! finds an album by barcode or Apple id and returns its artwork URL, and Apple's
//! image host renders that asset at any requested size.

use crate::musicbrainz;
use crate::net::{self, Policy, RateLimitWaiter};
use crate::state::State;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use ureq::Agent;

const LOOKUP_ENDPOINT: &str = "https://itunes.apple.com/lookup";
/// The Lookup API allows roughly 20 calls per minute.
const MIN_DELAY: Duration = Duration::from_secs(3);
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// Throttling comes back as `403` without a `Retry-After`.
const POLICY: Policy<'static> = Policy {
    service: "itunes lookup",
    max_rate_limit_retries: Some(5),
    default_wait_secs: (60, 10),
    throttle_statuses: &[403, 429, 503],
};
/// `<w>x<h>bb` fits inside the box, capped at the source size, so this is "the
/// largest Apple has" for any album.
pub const COVER_RENDITION: &str = "4000x4000bb.jpg";

/// An album as returned by the Lookup API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    pub name: String,
    /// Asset base any rendition can be appended to (see [`rendition`]).
    pub artwork_base: String,
}

pub struct Lookup {
    agent: Agent,
    user_agent: String,
    delay: Duration,
}

impl Lookup {
    pub fn new(user_agent: String, delay: Duration) -> Self {
        Self {
            agent: net::agent(),
            user_agent,
            delay: delay.max(MIN_DELAY),
        }
    }

    pub fn album_by_barcode<W: RateLimitWaiter>(
        &self,
        barcode: &str,
        waiter: &W,
    ) -> Result<Option<Album>> {
        self.lookup(("upc", barcode), waiter)
    }

    pub fn album_by_id<W: RateLimitWaiter>(&self, id: &str, waiter: &W) -> Result<Option<Album>> {
        self.lookup(("id", id), waiter)
    }

    fn lookup<W: RateLimitWaiter>(
        &self,
        (key, value): (&str, &str),
        waiter: &W,
    ) -> Result<Option<Album>> {
        let build = || {
            self.agent
                .get(LOOKUP_ENDPOINT)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "application/json")
                .query("entity", "album")
                .query(key, value)
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;
        match response.status().as_u16() {
            200 => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES)?;
                let value: Value = serde_json::from_str(&body)?;
                Ok(album_from(&value))
            }
            404 => Ok(None),
            other => bail!("itunes lookup returned {other}"),
        }
    }
}

/// The first album collection in a Lookup response that has artwork.
pub fn album_from(response: &Value) -> Option<Album> {
    response
        .get("results")?
        .as_array()?
        .iter()
        .filter(|r| r.get("wrapperType").and_then(Value::as_str) == Some("collection"))
        .find_map(|r| {
            let base = asset_base(r.get("artworkUrl100")?.as_str()?)?;
            let id = match r.get("collectionId")? {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => return None,
            };
            Some(Album {
                id,
                name: r
                    .get("collectionName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                artwork_base: base,
            })
        })
}

/// Digits only; UPC-A, EAN-13 and GTIN-14 are all accepted by the Lookup API.
pub fn normalize_barcode(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    (8..=14).contains(&digits.len()).then_some(digits)
}

/// Strip the rendition segment (`{w}x{h}{c}.{f}`, `100x100bb.jpg`, `1200x630cw.png`)
/// off an Apple image URL, leaving the base any rendition can be appended to.
pub fn asset_base(url: &str) -> Option<String> {
    let (base, last) = url.rsplit_once('/')?;
    let is_rendition = last.contains("{w}")
        || last.split_once('x').is_some_and(|(w, rest)| {
            !w.is_empty()
                && w.bytes().all(|b| b.is_ascii_digit())
                && rest.starts_with(|c: char| c.is_ascii_digit())
        });
    is_rendition.then(|| base.to_string())
}

pub fn rendition(base: &str, spec: &str) -> String {
    format!("{base}/{spec}")
}

// ---- Release artwork resolution ---------------------------------------------

pub const RELEASE_ART_VERSION: u32 = 1;

/// Where a release's Apple artwork was found — cached as `release/<mbid>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseArt {
    #[serde(default)]
    pub version: u32,
    pub album_id: String,
    pub album_name: String,
    pub artwork_url: String,
    /// How the album was matched: `barcode`, `musicbrainz barcode`,
    /// `musicbrainz link`, or `sibling release link`.
    pub via: String,
}

/// Find the Apple Music artwork for a release. Tries, in order: the barcode from
/// the file's tags; the barcode or Apple link on the `MusicBrainz` release; an
/// Apple link on any other release in the same release group. A hit is cached per
/// release; `None` records a miss.
#[allow(clippy::too_many_arguments)]
pub fn resolve_release_art<W: RateLimitWaiter>(
    state: &State,
    mb: &musicbrainz::Client,
    lookup: &Lookup,
    waiter: &W,
    release: &str,
    group: Option<&str>,
    tagged_barcode: Option<&str>,
    recheck: bool,
) -> Result<Option<ReleaseArt>> {
    if !recheck
        && let Some(art) = state.load_release_art::<ReleaseArt>(release)?
        && art.version == RELEASE_ART_VERSION
    {
        return Ok(Some(art));
    }

    let tagged = tagged_barcode.and_then(normalize_barcode);
    let mut found: Option<(Album, &str)> = None;
    if let Some(barcode) = &tagged {
        found = lookup
            .album_by_barcode(barcode, waiter)?
            .map(|a| (a, "barcode"));
    }
    if found.is_none() {
        found = via_release(
            state,
            mb,
            lookup,
            waiter,
            release,
            tagged.is_some(),
            recheck,
        )?;
    }
    if found.is_none()
        && let Some(group) = group
    {
        found = via_group(state, mb, lookup, waiter, group, recheck)?;
    }

    if let Some((album, via)) = found {
        let art = ReleaseArt {
            version: RELEASE_ART_VERSION,
            album_id: album.id,
            album_name: album.name,
            artwork_url: rendition(&album.artwork_base, COVER_RENDITION),
            via: via.to_string(),
        };
        state.save_release_art(release, &art)?;
        Ok(Some(art))
    } else {
        state.save_release_miss(release)?;
        Ok(None)
    }
}

/// Look the album up through the `MusicBrainz` release itself: its barcode (unless
/// the tags already supplied one that failed), then its Apple Music link.
fn via_release<W: RateLimitWaiter>(
    state: &State,
    mb: &musicbrainz::Client,
    lookup: &Lookup,
    waiter: &W,
    release: &str,
    barcode_tried: bool,
    recheck: bool,
) -> Result<Option<(Album, &'static str)>> {
    let cached = if recheck {
        None
    } else {
        state.load_musicbrainz_release(release)?
    };
    let Some(json) = cached_or_fetched(
        cached,
        || mb.release(release, waiter),
        |v| state.save_musicbrainz_release(release, v),
    )?
    else {
        return Ok(None);
    };
    if !barcode_tried
        && let Some(barcode) =
            musicbrainz::release_barcode(&json).and_then(|b| normalize_barcode(&b))
        && let Some(album) = lookup.album_by_barcode(&barcode, waiter)?
    {
        return Ok(Some((album, "musicbrainz barcode")));
    }
    if let Some(id) = musicbrainz::apple_album_link(&json)
        && let Some(album) = lookup.album_by_id(&id, waiter)?
    {
        return Ok(Some((album, "musicbrainz link")));
    }
    Ok(None)
}

/// Look the album up through an Apple Music link on any sibling release in the
/// release group (one browse request covers them all).
fn via_group<W: RateLimitWaiter>(
    state: &State,
    mb: &musicbrainz::Client,
    lookup: &Lookup,
    waiter: &W,
    group: &str,
    recheck: bool,
) -> Result<Option<(Album, &'static str)>> {
    let cached = if recheck {
        None
    } else {
        state.load_musicbrainz_group(group)?
    };
    let Some(browse) = cached_or_fetched(
        cached,
        || mb.releases_in_group(group, waiter),
        |v| state.save_musicbrainz_group(group, v),
    )?
    else {
        return Ok(None);
    };
    if let Some(id) = musicbrainz::apple_album_in_group(&browse)
        && let Some(album) = lookup.album_by_id(&id, waiter)?
    {
        return Ok(Some((album, "sibling release link")));
    }
    Ok(None)
}

/// A cached JSON document, or the freshly fetched one (saved for next time).
fn cached_or_fetched(
    cached: Option<Value>,
    fetch: impl FnOnce() -> Result<Option<Value>>,
    save: impl FnOnce(&Value) -> Result<()>,
) -> Result<Option<Value>> {
    if cached.is_some() {
        return Ok(cached);
    }
    let value = fetch()?;
    if let Some(value) = &value {
        save(value)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str =
        "https://is1-ssl.mzstatic.com/image/thumb/Music116/v4/07/60/ba/x/634904078164.png";

    #[test]
    fn asset_base_strips_renditions_only() {
        assert_eq!(
            asset_base(&format!("{BASE}/{{w}}x{{h}}{{c}}.{{f}}")).as_deref(),
            Some(BASE)
        );
        assert_eq!(
            asset_base(&format!("{BASE}/100x100bb.jpg")).as_deref(),
            Some(BASE)
        );
        assert_eq!(
            asset_base(&format!("{BASE}/1200x630cw.png")).as_deref(),
            Some(BASE)
        );
        assert_eq!(asset_base(BASE), None);
        assert_eq!(
            rendition(BASE, COVER_RENDITION),
            format!("{BASE}/4000x4000bb.jpg")
        );
    }

    #[test]
    fn parses_lookup_responses() {
        let response = json!({
            "resultCount": 2,
            "results": [
                { "wrapperType": "artist", "artistName": "Radiohead" },
                { "wrapperType": "collection", "collectionType": "Album",
                  "collectionId": 1_097_861_387, "collectionName": "OK Computer",
                  "artworkUrl100": format!("{BASE}/100x100bb.jpg") }
            ]
        });
        assert_eq!(
            album_from(&response),
            Some(Album {
                id: "1097861387".into(),
                name: "OK Computer".into(),
                artwork_base: BASE.into()
            })
        );
        assert_eq!(
            album_from(&json!({ "resultCount": 0, "results": [] })),
            None
        );
    }

    #[test]
    fn normalizes_barcodes() {
        assert_eq!(
            normalize_barcode(" 0634904078164 ").as_deref(),
            Some("0634904078164")
        );
        assert_eq!(
            normalize_barcode("7243-8552-29-49").as_deref(),
            Some("724385522949")
        );
        assert_eq!(normalize_barcode("none"), None);
        assert_eq!(normalize_barcode("123"), None);
    }
}
