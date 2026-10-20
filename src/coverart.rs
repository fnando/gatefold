use crate::net::{RateLimitWaiter, retry_after, with_jitter};
use anyhow::{Result, anyhow, bail};
use std::thread::sleep;
use std::time::Duration;
use ureq::Agent;

const BASE: &str = "https://coverartarchive.org";
/// The front-cover variant to fetch. `front-1200` is the CAA cover scaled to fit
/// 1200px (or the original if it's already smaller) — a player-friendly size.
/// It's part of the cache key, so changing it re-fetches.
pub const FRONT_VARIANT: &str = "front-1200";
/// Cover Art Archive images are served from archive.org and can be large.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Unlike lrclib, back off a bounded number of times so a sustained outage
/// (503s that aren't really a rate limit) can't hang the whole run.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;

/// A downloaded cover image.
pub struct Image {
    pub mime: String,
    pub ext: String,
    pub data: Vec<u8>,
}

/// Outcome of a lookup that completed without a hard error.
pub enum Fetched {
    Found(Image),
    NotFound,
}

pub struct Client {
    agent: Agent,
    user_agent: String,
    delay: Duration,
}

impl Client {
    pub fn new(user_agent: String, delay: Duration) -> Self {
        // Follow the 307 to archive.org automatically; don't treat 4xx/5xx as
        // errors so we can read status + Retry-After ourselves.
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(10)
            .max_redirects_will_error(false)
            .build()
            .into();
        Self {
            agent,
            user_agent,
            delay,
        }
    }

    /// Fetch the front cover for a `MusicBrainz` entity. `entity` is `"release"`
    /// or `"release-group"`; `mbid` is the corresponding id.
    pub fn front<W: RateLimitWaiter>(
        &self,
        entity: &str,
        mbid: &str,
        waiter: &W,
    ) -> Result<Fetched> {
        let url = format!("{BASE}/{entity}/{mbid}/{FRONT_VARIANT}");
        let mut throttle_retries = 0;

        loop {
            sleep(self.delay);

            let mut response = self
                .agent
                .get(&url)
                .header("User-Agent", self.user_agent.as_str())
                .call()
                .map_err(|e| anyhow!("request failed: {e}"))?;
            let status = response.status().as_u16();

            match status {
                200 => {
                    let mime = content_type(&response);
                    let data = response
                        .body_mut()
                        .with_config()
                        .limit(MAX_IMAGE_BYTES)
                        .read_to_vec()
                        .map_err(|e| anyhow!("reading image: {e}"))?;
                    if data.is_empty() {
                        bail!("cover art archive returned an empty image");
                    }
                    let ext = extension_for(&mime);
                    return Ok(Fetched::Found(Image { mime, ext, data }));
                }
                404 => return Ok(Fetched::NotFound),
                429 | 503 => {
                    throttle_retries += 1;
                    if throttle_retries > MAX_RATE_LIMIT_RETRIES {
                        bail!(
                            "cover art archive still unavailable after {MAX_RATE_LIMIT_RETRIES} retries"
                        );
                    }
                    // 503 doubles as the rate-limit signal and may omit Retry-After.
                    let wait = retry_after(&response).unwrap_or(if status == 503 { 3 } else { 5 });
                    waiter.wait(with_jitter(wait), "cover art archive busy");
                }
                other => bail!("cover art archive returned {other}"),
            }
        }
    }
}

/// Extract a bare content-type (without parameters) from the response.
fn content_type(response: &ureq::http::Response<ureq::Body>) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map_or_else(
            || "image/jpeg".to_string(),
            |v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase(),
        )
}

fn extension_for(mime: &str) -> String {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
    .to_string()
}
