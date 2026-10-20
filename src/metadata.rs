use crate::writer::{tag_has_front_cover, tag_has_lyrics};
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::read_from_path;
use lofty::tag::ItemKey;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped whenever the set of extracted fields changes, so cached entries from
/// an older build are re-parsed instead of silently missing new data.
pub const CURRENT_VERSION: u32 = 1;

/// Tags extracted from a track, cached so files aren't re-parsed on later runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMetadata {
    /// Schema version of this cached entry (see [`CURRENT_VERSION`]).
    #[serde(default)]
    pub version: u32,
    pub path: String,
    pub track_name: Option<String>,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    /// Duration in whole seconds (0 / None when unknown).
    pub duration: Option<u64>,
    /// Whether the file already carries a non-empty lyrics tag.
    pub has_lyrics_tag: bool,
    /// `MusicBrainz` release (album) id, used to look up cover art without an API call.
    #[serde(default)]
    pub musicbrainz_release_id: Option<String>,
    /// `MusicBrainz` release-group id, used as a cover-art fallback when the
    /// specific release has no art on the Cover Art Archive.
    #[serde(default)]
    pub musicbrainz_release_group_id: Option<String>,
    /// Whether the file already embeds a front-cover picture.
    #[serde(default)]
    pub has_cover_art: bool,
}

impl TrackMetadata {
    /// True when we have enough to query lrclib (`track_name` + `artist_name`).
    pub fn is_queryable(&self) -> bool {
        non_empty(self.track_name.as_deref()) && non_empty(self.artist_name.as_deref())
    }

    /// True when this cached entry matches the current extraction schema.
    pub fn is_current(&self) -> bool {
        self.version == CURRENT_VERSION
    }
}

fn non_empty(value: Option<&str>) -> bool {
    value.is_some_and(|s| !s.trim().is_empty())
}

/// Read a non-empty string value for `key` from the tag.
fn string_tag(tag: Option<&lofty::tag::Tag>, key: ItemKey) -> Option<String> {
    tag.and_then(|t| t.get_string(key))
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Read tags and duration from the file at `path` via lofty.
pub fn extract(path: &Path) -> Result<TrackMetadata> {
    let tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;

    let duration = {
        let secs = tagged.properties().duration().as_secs();
        if secs == 0 { None } else { Some(secs) }
    };

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let track_name = tag.and_then(Accessor::title).map(|c| c.to_string());
    let artist_name = tag.and_then(Accessor::artist).map(|c| c.to_string());
    let album_name = tag.and_then(Accessor::album).map(|c| c.to_string());
    let has_lyrics_tag = tag.is_some_and(tag_has_lyrics);
    let musicbrainz_release_id = string_tag(tag, ItemKey::MusicBrainzReleaseId);
    let musicbrainz_release_group_id = string_tag(tag, ItemKey::MusicBrainzReleaseGroupId);
    let has_cover_art = tag.is_some_and(tag_has_front_cover);

    Ok(TrackMetadata {
        version: CURRENT_VERSION,
        path: path.to_string_lossy().into_owned(),
        track_name,
        artist_name,
        album_name,
        duration,
        has_lyrics_tag,
        musicbrainz_release_id,
        musicbrainz_release_group_id,
        has_cover_art,
    })
}
