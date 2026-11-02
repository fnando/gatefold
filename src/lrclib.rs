use crate::net::{self, Policy, RateLimitWaiter};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use ureq::Agent;

const ENDPOINT: &str = "https://lrclib.net/api/get";
/// lrclib's `Retry-After` is authoritative: keep waiting for as long as it asks.
const POLICY: Policy<'static> = Policy {
    service: "lrclib",
    max_rate_limit_retries: None,
    default_wait_secs: (5, 1),
    throttle_statuses: net::THROTTLE,
};
/// Lyrics responses are small; anything bigger is not what we asked for.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// A lyrics record as returned by lrclib. Serialized verbatim into the cache,
/// so field names stay camelCase to match the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsRecord {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub track_name: Option<String>,
    #[serde(default)]
    pub artist_name: Option<String>,
    #[serde(default)]
    pub album_name: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    #[serde(default)]
    pub synced_lyrics: Option<String>,
}

impl LyricsRecord {
    pub fn has_synced(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    pub fn has_plain(&self) -> bool {
        self.plain_lyrics
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

/// Outcome of a lookup that completed without a hard error.
pub enum Fetched {
    Found(LyricsRecord),
    NotFound,
}

/// Returned on `401`; the caller should abort the whole run.
#[derive(Debug)]
pub struct Unauthorized(pub String);

impl fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unauthorized: {}", self.0)
    }
}

impl std::error::Error for Unauthorized {}

/// Query inputs extracted from a track's tags.
pub struct Query<'a> {
    pub track_name: &'a str,
    pub artist_name: &'a str,
    pub album_name: Option<&'a str>,
    pub duration: Option<u64>,
}

pub struct Client {
    agent: Agent,
    user_agent: String,
    app_id: Option<String>,
    api_key: Option<String>,
    delay: Duration,
}

impl Client {
    pub fn new(
        user_agent: String,
        app_id: Option<String>,
        api_key: Option<String>,
        delay: Duration,
    ) -> Self {
        Self {
            agent: net::agent(),
            user_agent,
            app_id,
            api_key,
            delay,
        }
    }

    /// Fetch lyrics, transparently retrying `429`/`503` by honoring `Retry-After`.
    pub fn get<W: RateLimitWaiter>(&self, query: &Query<'_>, waiter: &W) -> Result<Fetched> {
        let build = || {
            let mut request = self
                .agent
                .get(ENDPOINT)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "application/json")
                .query("track_name", query.track_name)
                .query("artist_name", query.artist_name);

            if let Some(album) = query.album_name {
                request = request.query("album_name", album);
            }
            if let Some(duration) = query.duration.filter(|d| (1..=3600).contains(d)) {
                request = request.query("duration", duration.to_string());
            }
            if let Some(app_id) = &self.app_id {
                request = request.header("Lrclib-App-Id", app_id.as_str());
            }
            if let Some(api_key) = &self.api_key {
                request = request.header("Lrclib-Api-Key", api_key.as_str());
            }
            request
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;

        match response.status().as_u16() {
            200 => {
                let is_json = net::content_type(&response, "").contains("json");
                let body = net::read_text(&mut response, MAX_BODY_BYTES)?;
                if !is_json {
                    bail!("unexpected non-JSON 200 response");
                }
                let record: LyricsRecord = serde_json::from_str(&body)?;
                Ok(Fetched::Found(record))
            }
            404 => Ok(Fetched::NotFound),
            401 => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES).unwrap_or_default();
                Err(anyhow::Error::new(Unauthorized(summarize(&body))))
            }
            other => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES).unwrap_or_default();
                bail!("lrclib returned {other}: {}", summarize(&body));
            }
        }
    }
}

fn summarize(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 200 {
        format!("{}…", &trimmed[..200])
    } else {
        trimmed.to_string()
    }
}
