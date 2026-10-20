use crate::net::{RateLimitWaiter, retry_after, with_jitter};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::thread::sleep;
use std::time::Duration;
use ureq::Agent;

const ENDPOINT: &str = "https://lrclib.net/api/get";

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
        // Disable "status >= 400 is an error" so we can inspect 404/429/503 responses.
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            user_agent,
            app_id,
            api_key,
            delay,
        }
    }

    /// Fetch lyrics, transparently retrying `429`/`503` by honoring `Retry-After`.
    pub fn get<W: RateLimitWaiter>(&self, query: &Query<'_>, waiter: &W) -> Result<Fetched> {
        loop {
            sleep(self.delay);

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

            let mut response = request.call().map_err(|e| anyhow!("request failed: {e}"))?;
            let status = response.status().as_u16();

            match status {
                200 => {
                    let is_json = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.contains("json"));
                    let body = response
                        .body_mut()
                        .read_to_string()
                        .map_err(|e| anyhow!("reading body: {e}"))?;
                    if !is_json {
                        bail!("unexpected non-JSON 200 response");
                    }
                    let record: LyricsRecord = serde_json::from_str(&body)?;
                    return Ok(Fetched::Found(record));
                }
                404 => return Ok(Fetched::NotFound),
                401 => {
                    let body = response.body_mut().read_to_string().unwrap_or_default();
                    return Err(anyhow::Error::new(Unauthorized(summarize(&body))));
                }
                429 | 503 => {
                    let default = if status == 503 { 1 } else { 5 };
                    let wait = retry_after(&response).unwrap_or(default);
                    let reason = if status == 503 {
                        "server overloaded"
                    } else {
                        "rate limited"
                    };
                    waiter.wait(with_jitter(wait), reason);
                }
                other => {
                    let body = response.body_mut().read_to_string().unwrap_or_default();
                    bail!("lrclib returned {other}: {}", summarize(&body));
                }
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
