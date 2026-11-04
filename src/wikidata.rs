//! Wikidata as a second source of streaming links: when `MusicBrainz` has no
//! Apple Music link for an artist, the artist's Wikidata item (found by its
//! `MusicBrainz` id, or the item `MusicBrainz` links to) often carries one as
//! "Apple Music artist ID" (P2850), and a Spotify id as P1902. One SPARQL query
//! against the public endpoint fetches both.

use crate::musicbrainz::ArtistLinks;
use crate::net::{self, Policy, RateLimitWaiter};
use anyhow::{Result, bail};
use serde_json::Value;
use std::time::Duration;
use ureq::Agent;

const SPARQL_ENDPOINT: &str = "https://query.wikidata.org/sparql";
const MAX_BODY_BYTES: u64 = 1024 * 1024;
/// The query service throttles with `429` + `Retry-After` and asks for a
/// descriptive `User-Agent`.
const POLICY: Policy<'static> = Policy {
    service: "wikidata",
    max_rate_limit_retries: Some(5),
    default_wait_secs: (30, 10),
    throttle_statuses: net::THROTTLE,
};

pub struct Client {
    agent: Agent,
    user_agent: String,
    delay: Duration,
}

impl Client {
    pub fn new(user_agent: String, delay: Duration) -> Self {
        Self {
            agent: net::agent(),
            user_agent,
            delay,
        }
    }

    /// The streaming ids of the artist's Wikidata item: `qid` when `MusicBrainz`
    /// links one, else the item whose "`MusicBrainz` artist ID" is `mbid`. The raw
    /// SPARQL result (possibly with no rows) is returned for caching; `None` only
    /// when the ids can't be put in a query.
    pub fn artist_links<W: RateLimitWaiter>(
        &self,
        mbid: &str,
        qid: Option<&str>,
        waiter: &W,
    ) -> Result<Option<Value>> {
        let Some(query) = query_for(mbid, qid) else {
            return Ok(None);
        };
        let build = || {
            self.agent
                .get(SPARQL_ENDPOINT)
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "application/sparql-results+json")
                .query("query", &query)
        };
        let mut response = net::get(build, self.delay, &POLICY, waiter)?;
        match response.status().as_u16() {
            200 => {
                let body = net::read_text(&mut response, MAX_BODY_BYTES)?;
                Ok(Some(serde_json::from_str(&body)?))
            }
            other => bail!("wikidata returned {other}"),
        }
    }
}

/// The SPARQL query for an artist's streaming ids. `None` when `mbid` isn't a
/// `MusicBrainz` id or `qid` isn't a Wikidata item id, so nothing unexpected is
/// ever interpolated into the query.
pub fn query_for(mbid: &str, qid: Option<&str>) -> Option<String> {
    let subject = if let Some(qid) = qid {
        let digits = qid.strip_prefix('Q')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        format!("VALUES ?item {{ wd:{qid} }}")
    } else {
        if mbid.is_empty() || !mbid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return None;
        }
        format!("?item wdt:P434 \"{mbid}\" .")
    };
    Some(format!(
        "SELECT ?apple ?spotify WHERE {{ {subject} OPTIONAL {{ ?item wdt:P2850 ?apple }} OPTIONAL {{ ?item wdt:P1902 ?spotify }} }} LIMIT 5"
    ))
}

/// The Apple Music / Spotify artist ids in a SPARQL result (first well-formed
/// value of each).
pub fn links_from(result: &Value) -> ArtistLinks {
    let mut links = ArtistLinks::default();
    let Some(rows) = result
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Value::as_array)
    else {
        return links;
    };
    for row in rows {
        let value = |name: &str| {
            row.get(name)
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        if links.apple_id.is_none()
            && let Some(id) = value("apple").filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        {
            links.apple_id = Some(id.to_string());
        }
        if links.spotify_id.is_none()
            && let Some(id) =
                value("spotify").filter(|v| v.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            links.spotify_id = Some(id.to_string());
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_queries_for_well_formed_ids_only() {
        let by_mbid = query_for("a74b1b7f-71a5-4011-9441-d0b5e4122711", None).unwrap();
        assert!(by_mbid.contains("?item wdt:P434 \"a74b1b7f-71a5-4011-9441-d0b5e4122711\""));
        let by_qid = query_for("a74b1b7f-71a5-4011-9441-d0b5e4122711", Some("Q44190")).unwrap();
        assert!(by_qid.contains("VALUES ?item { wd:Q44190 }"));
        assert_eq!(query_for("not an id\" }", None), None);
        assert_eq!(query_for("a74b1b7f", Some("Q44190 }")), None);
        assert_eq!(query_for("", None), None);
    }

    #[test]
    fn reads_ids_out_of_sparql_rows() {
        let result = json!({
            "head": { "vars": ["apple", "spotify"] },
            "results": { "bindings": [
                { "spotify": { "type": "literal", "value": "4Z8W4fKeB5YxbusRsdQVPb" } },
                { "apple": { "type": "literal", "value": "657515" },
                  "spotify": { "type": "literal", "value": "ignored" } }
            ] }
        });
        let links = links_from(&result);
        assert_eq!(links.apple_id.as_deref(), Some("657515"));
        assert_eq!(links.spotify_id.as_deref(), Some("4Z8W4fKeB5YxbusRsdQVPb"));

        let junk = json!({ "results": { "bindings": [
            { "apple": { "value": "https://music.apple.com/artist/1" } }
        ] } });
        assert!(links_from(&junk).is_empty());
        assert!(links_from(&json!({ "results": { "bindings": [] } })).is_empty());
        assert!(links_from(&json!({})).is_empty());
    }
}
