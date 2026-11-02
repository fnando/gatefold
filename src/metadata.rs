use crate::writer::{tag_has_front_cover, tag_has_lyrics};
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::read_from_path;
use lofty::tag::ItemKey;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped whenever the set of extracted fields changes, so cached entries from
/// an older build are re-parsed instead of silently missing new data.
pub const CURRENT_VERSION: u32 = 3;

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
    /// Album artist as tagged; falls back to the track artist via [`Self::album_artist`].
    #[serde(default)]
    pub album_artist_name: Option<String>,
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
    /// `MusicBrainz` album-artist id (falling back to the track artist id), used
    /// to look up the artist's streaming-service pages for artist images.
    #[serde(default)]
    pub musicbrainz_album_artist_id: Option<String>,
    /// Whether the file already embeds a front-cover picture.
    #[serde(default)]
    pub has_cover_art: bool,
    /// Release barcode (UPC/EAN) as tagged, used to find the album on Apple Music.
    #[serde(default)]
    pub barcode: Option<String>,
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

    /// The album artist, or the track artist when no album artist is tagged.
    pub fn album_artist(&self) -> Option<&str> {
        [&self.album_artist_name, &self.artist_name]
            .into_iter()
            .filter_map(|v| v.as_deref())
            .map(str::trim)
            .find(|s| !s.is_empty())
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

/// Some taggers join several ids with `/`, `;` or `,` (collaborations); keep the
/// first token that looks like a UUID, or nothing if none does.
fn first_mbid(value: &str) -> Option<String> {
    value
        .split(['/', ';', ','])
        .map(str::trim)
        .find(|s| is_uuid(s))
        .map(str::to_ascii_lowercase)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
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
    let album_artist_name = string_tag(tag, ItemKey::AlbumArtist);
    let has_lyrics_tag = tag.is_some_and(tag_has_lyrics);
    let musicbrainz_release_id = string_tag(tag, ItemKey::MusicBrainzReleaseId);
    let musicbrainz_release_group_id = string_tag(tag, ItemKey::MusicBrainzReleaseGroupId);
    let musicbrainz_album_artist_id = string_tag(tag, ItemKey::MusicBrainzReleaseArtistId)
        .or_else(|| string_tag(tag, ItemKey::MusicBrainzArtistId))
        .and_then(|v| first_mbid(&v));
    let has_cover_art = tag.is_some_and(tag_has_front_cover);
    let barcode = string_tag(tag, ItemKey::Barcode);

    Ok(TrackMetadata {
        version: CURRENT_VERSION,
        path: path.to_string_lossy().into_owned(),
        track_name,
        artist_name,
        album_name,
        album_artist_name,
        duration,
        has_lyrics_tag,
        musicbrainz_release_id,
        musicbrainz_release_group_id,
        musicbrainz_album_artist_id,
        has_cover_art,
        barcode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_mbid_keeps_the_first_uuid() {
        let id = "A74B1B7F-71A5-4011-9441-D0B5E4122711";
        assert_eq!(
            first_mbid(id).as_deref(),
            Some("a74b1b7f-71a5-4011-9441-d0b5e4122711")
        );
        assert_eq!(
            first_mbid(&format!(
                "junk / {id}; 00000000-0000-0000-0000-000000000000"
            ))
            .as_deref(),
            Some("a74b1b7f-71a5-4011-9441-d0b5e4122711")
        );
        assert_eq!(first_mbid("not-an-id"), None);
        assert_eq!(first_mbid(""), None);
    }

    #[test]
    fn album_artist_falls_back_to_track_artist() {
        let mut meta = TrackMetadata {
            version: CURRENT_VERSION,
            path: String::new(),
            track_name: None,
            artist_name: Some("Thom Yorke".into()),
            album_name: None,
            album_artist_name: Some("  ".into()),
            duration: None,
            has_lyrics_tag: false,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            musicbrainz_album_artist_id: None,
            has_cover_art: false,
            barcode: None,
        };
        assert_eq!(meta.album_artist(), Some("Thom Yorke"));
        meta.album_artist_name = Some("Radiohead".into());
        assert_eq!(meta.album_artist(), Some("Radiohead"));
    }
}
