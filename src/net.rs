use rand::RngExt;
use std::time::Duration;

/// Something that can block (with a visible spinner) while we respect a `Retry-After`.
pub trait RateLimitWaiter {
    fn wait(&self, duration: Duration, reason: &str);
}

/// Parse a `Retry-After` header expressed as a whole number of seconds.
pub fn retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<u32> {
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
