use crate::net::{self, Image, Policy, RateLimitWaiter};
use anyhow::{Result, bail};
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
const POLICY: Policy<'static> = Policy {
    service: "cover art archive",
    max_rate_limit_retries: Some(5),
    default_wait_secs: (5, 3),
    throttle_statuses: net::THROTTLE,
};

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
        Self {
            // Follows the 307 to archive.org automatically.
            agent: net::agent(),
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
        let build = || {
            self.agent
                .get(&url)
                .header("User-Agent", self.user_agent.as_str())
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;

        match response.status().as_u16() {
            200 => {
                let mime = net::content_type(&response, "image/jpeg");
                let data = net::read_limited(&mut response, MAX_IMAGE_BYTES)?;
                if data.is_empty() {
                    bail!("cover art archive returned an empty image");
                }
                let ext = net::extension_for(&mime);
                Ok(Fetched::Found(Image { mime, ext, data }))
            }
            404 => Ok(Fetched::NotFound),
            other => bail!("cover art archive returned {other}"),
        }
    }
}
