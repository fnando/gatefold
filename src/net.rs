use anyhow::{Result, anyhow, bail};
use rand::RngExt;
use std::thread::sleep;
use std::time::Duration;
use ureq::http::Response;
use ureq::typestate::WithoutBody;
use ureq::{Agent, Body, RequestBuilder};

/// Something that can block (with a visible spinner) while we respect a `Retry-After`.
pub trait RateLimitWaiter {
    fn wait(&self, duration: Duration, reason: &str);
}

/// A downloaded image: bare MIME type, a matching file extension, and the bytes.
pub struct Image {
    pub mime: String,
    pub ext: String,
    pub data: Vec<u8>,
}

/// How a service's throttling signals are handled by [`get`].
pub struct Policy<'a> {
    /// Short name used in error and spinner messages (e.g. `"lrclib"`).
    pub service: &'a str,
    /// Give up after this many consecutive `429`/`503` responses; `None` keeps
    /// retrying for as long as the service keeps asking us to wait.
    pub max_rate_limit_retries: Option<u32>,
    /// Seconds to wait when `Retry-After` is absent, for `429` and `503` respectively
    /// (any other throttle status uses the `429` value).
    pub default_wait_secs: (u32, u32),
    /// Statuses that mean "slow down" rather than failure; usually [`THROTTLE`].
    pub throttle_statuses: &'static [u16],
}

/// The usual throttling signals.
pub const THROTTLE: &[u16] = &[429, 503];

/// An agent that follows redirects and hands 4xx/5xx responses back to us (so
/// status codes and `Retry-After` can be inspected) instead of raising them.
pub fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(10)
        .max_redirects_will_error(false)
        .build()
        .into()
}

/// Sleep `delay`, issue the request produced by `build`, and transparently retry
/// `429`/`503` by honoring `Retry-After` (with jitter) through `waiter`. Every
/// other response, success or failure, is returned for the caller to match on.
/// `build` is a closure because a `RequestBuilder` can't be cloned for a retry.
pub fn get<W: RateLimitWaiter>(
    build: impl Fn() -> RequestBuilder<WithoutBody>,
    delay: Duration,
    policy: &Policy<'_>,
    waiter: &W,
) -> Result<Response<Body>> {
    let mut throttle_retries = 0;
    loop {
        sleep(delay);

        let response = build().call().map_err(|e| anyhow!("request failed: {e}"))?;
        let status = response.status().as_u16();
        if !policy.throttle_statuses.contains(&status) {
            return Ok(response);
        }

        throttle_retries += 1;
        if let Some(max) = policy.max_rate_limit_retries
            && throttle_retries > max
        {
            bail!("{} still unavailable after {max} retries", policy.service);
        }
        // 503 doubles as a rate-limit signal on some services and may omit Retry-After.
        let (on_429, on_503) = policy.default_wait_secs;
        let wait = retry_after(&response).unwrap_or(if status == 503 { on_503 } else { on_429 });
        let reason = if status == 503 {
            format!("{} overloaded", policy.service)
        } else {
            format!("{} rate limited", policy.service)
        };
        waiter.wait(with_jitter(wait), &reason);
    }
}

/// Read a response body into memory, refusing anything over `limit` bytes.
pub fn read_limited(response: &mut Response<Body>, limit: u64) -> Result<Vec<u8>> {
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| anyhow!("reading body: {e}"))
}

/// Read a response body as text, refusing anything over `limit` bytes.
pub fn read_text(response: &mut Response<Body>, limit: u64) -> Result<String> {
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .map_err(|e| anyhow!("reading body: {e}"))
}

/// Extract a bare content-type (without parameters) from the response, falling
/// back to `default` when the header is missing.
pub fn content_type(response: &Response<Body>, default: &str) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map_or_else(
            || default.to_string(),
            |v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase(),
        )
}

/// File extension for an image MIME type, defaulting to `jpg`.
pub fn extension_for(mime: &str) -> String {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
    .to_string()
}

/// Parse a `Retry-After` header expressed as a whole number of seconds.
pub fn retry_after(response: &Response<Body>) -> Option<u32> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Add up to 30% random jitter so throttled clients don't retry in lockstep.
pub fn with_jitter(seconds: u32) -> Duration {
    let base = f64::from(seconds);
    let jitter = base * 0.30 * rand::rng().random::<f64>();
    Duration::from_secs_f64(base + jitter)
}
