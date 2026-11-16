use crate::writer::{tag_has_front_cover, tag_has_lyrics};
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::read_from_path;
use lofty::tag::ItemKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Bumped whenever the set of extracted fields changes, so cached entries from
/// an older build are re-parsed instead of silently missing new data.
pub const CURRENT_VERSION: u32 = 5;

/// Cheap identity of a file's contents: its modification time and size, both
/// from a single `stat`. A cached snapshot whose fingerprint no longer matches
/// the file (re-tagged, re-uploaded, or rewritten by us) is re-extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Modification time as nanoseconds since the Unix epoch.
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
}

impl Fingerprint {
    /// Stat `path` and read its fingerprint.
    pub fn of(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let modified = meta
            .modified()
            .with_context(|| format!("reading mtime of {}", path.display()))?;
        let mtime = modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        Ok(Self {
            mtime,
            size: meta.len(),
        })
    }
}

/// Tags extracted from a track, cached so files aren't re-parsed on later runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMetadata {
    /// Schema version of this cached entry (see [`CURRENT_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// mtime + size of the file when these tags were read (see [`Fingerprint`]).
    #[serde(default)]
    pub fingerprint: Option<Fingerprint>,
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
    /// Everything else about the release and the track, for `album.nfo`.
    #[serde(default)]
    pub release: ReleaseTags,
}

/// The rest of what the tags say about the release and the track's place in
/// it (as Picard writes them), for `album.nfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseTags {
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    /// Release date as tagged (`DATE`): `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
    pub date: Option<String>,
    /// First release of the release group (`ORIGINALDATE`).
    pub original_date: Option<String>,
    pub genres: Vec<String>,
    pub labels: Vec<String>,
    pub catalog_number: Option<String>,
    /// `album`, `ep`, `single`, … (`RELEASETYPE`).
    pub release_type: Option<String>,
    pub release_country: Option<String>,
    /// `CD`, `Digital Media`, `12" Vinyl`, … (`MEDIA`).
    pub media: Option<String>,
    pub compilation: Option<bool>,
    /// Each album artist (`ALBUMARTISTS`), when tagged separately.
    pub album_artists: Vec<String>,
    /// Every `MusicBrainz` album-artist id, not just the first.
    pub album_artist_ids: Vec<String>,
    /// `MusicBrainz` recording id (Picard's `MUSICBRAINZ_TRACKID`).
    pub recording_id: Option<String>,
    /// `MusicBrainz` release-track id (`MUSICBRAINZ_RELEASETRACKID`).
    pub release_track_id: Option<String>,
    pub isrc: Option<String>,
}

impl TrackMetadata {
    /// True when we have enough to query lrclib (`track_name` + `artist_name`).
    pub fn is_queryable(&self) -> bool {
        non_empty(self.track_name.as_deref()) && non_empty(self.artist_name.as_deref())
    }

    /// True when this cached entry matches the current extraction schema and
    /// was read from a file with this exact `fingerprint`.
    pub fn is_current(&self, fingerprint: &Fingerprint) -> bool {
        self.version == CURRENT_VERSION && self.fingerprint.as_ref() == Some(fingerprint)
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

/// Read tags and duration from the file at `path` via lofty, recording the
/// file's fingerprint so later runs can tell when it has changed.
pub fn extract(path: &Path) -> Result<TrackMetadata> {
    // Stat first: if the file is rewritten mid-extraction, the stale fingerprint
    // forces a re-read next run rather than caching the new file's tags under
    // the old one's identity.
    let fingerprint = Fingerprint::of(path)?;
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
    let release = tag.map_or_default(release_tags);

    Ok(TrackMetadata {
        version: CURRENT_VERSION,
        fingerprint: Some(fingerprint),
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
        release,
    })
}

fn release_tags(tag: &lofty::tag::Tag) -> ReleaseTags {
    let one = |key| string_tag(Some(tag), key);
    let ids = |key| tag.get_strings(key).flat_map(mbids).collect::<Vec<_>>();
    let mut album_artist_ids = ids(ItemKey::MusicBrainzReleaseArtistId);
    if album_artist_ids.is_empty() {
        album_artist_ids = ids(ItemKey::MusicBrainzArtistId);
    }
    ReleaseTags {
        track_number: tag.track(),
        track_total: tag.track_total(),
        disc_number: tag.disk(),
        disc_total: tag.disk_total(),
        date: one(ItemKey::RecordingDate)
            .or_else(|| one(ItemKey::ReleaseDate))
            .or_else(|| one(ItemKey::Year)),
        original_date: one(ItemKey::OriginalReleaseDate),
        genres: many(tag, ItemKey::Genre),
        labels: many(tag, ItemKey::Label),
        catalog_number: one(ItemKey::CatalogNumber),
        release_type: one(ItemKey::MusicBrainzReleaseType),
        release_country: one(ItemKey::ReleaseCountry),
        media: one(ItemKey::OriginalMediaType),
        compilation: one(ItemKey::FlagCompilation)
            .map(|v| matches!(v.trim(), "1" | "true" | "True" | "TRUE")),
        album_artists: many(tag, ItemKey::AlbumArtists),
        album_artist_ids,
        recording_id: one(ItemKey::MusicBrainzRecordingId).and_then(|v| first_mbid(&v)),
        release_track_id: one(ItemKey::MusicBrainzTrackId).and_then(|v| first_mbid(&v)),
        isrc: one(ItemKey::Isrc),
    }
}

/// Every value for `key`, also split on `;` (how some taggers join several
/// into one), trimmed, without empties or repeats.
fn many(tag: &lofty::tag::Tag, key: ItemKey) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for value in tag.get_strings(key).flat_map(|v| v.split(';')) {
        let value = value.trim();
        if !value.is_empty() && !values.iter().any(|seen| seen == value) {
            values.push(value.to_string());
        }
    }
    values
}

/// Every UUID-looking token in `value` (see [`first_mbid`]), lowercased.
fn mbids(value: &str) -> Vec<String> {
    value
        .split(['/', ';', ','])
        .map(str::trim)
        .filter(|s| is_uuid(s))
        .map(str::to_ascii_lowercase)
        .collect()
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
    fn release_tags_round_trip_and_old_entries_default() {
        let release = ReleaseTags {
            track_number: Some(3),
            disc_number: Some(2),
            genres: vec!["Punk".into(), "Ska".into()],
            album_artist_ids: vec!["a74b1b7f-71a5-4011-9441-d0b5e4122711".into()],
            compilation: Some(false),
            ..ReleaseTags::default()
        };
        let json = serde_json::to_string(&release).unwrap();
        assert_eq!(serde_json::from_str::<ReleaseTags>(&json).unwrap(), release);
        // An entry cached before these fields existed reads as empty.
        let old: TrackMetadata = serde_json::from_str(
            r#"{"version":4,"path":"x","track_name":null,"artist_name":null,"album_name":null,"duration":null,"has_lyrics_tag":false}"#,
        )
        .unwrap();
        assert_eq!(old.release, ReleaseTags::default());
        assert_eq!(
            mbids(
                "A74B1B7F-71A5-4011-9441-D0B5E4122711; junk/00000000-0000-0000-0000-000000000000"
            ),
            [
                "a74b1b7f-71a5-4011-9441-d0b5e4122711",
                "00000000-0000-0000-0000-000000000000"
            ]
        );
    }

    #[test]
    fn is_current_requires_matching_version_and_fingerprint() {
        let fp = Fingerprint { mtime: 1, size: 2 };
        let mut meta = TrackMetadata {
            version: CURRENT_VERSION,
            fingerprint: Some(fp),
            path: String::new(),
            track_name: None,
            artist_name: None,
            album_name: None,
            album_artist_name: None,
            duration: None,
            has_lyrics_tag: false,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            musicbrainz_album_artist_id: None,
            has_cover_art: false,
            barcode: None,
            release: ReleaseTags::default(),
        };
        assert!(meta.is_current(&fp));
        assert!(!meta.is_current(&Fingerprint { mtime: 1, size: 3 }));
        assert!(!meta.is_current(&Fingerprint { mtime: 9, size: 2 }));
        meta.fingerprint = None;
        assert!(!meta.is_current(&fp));
        meta.fingerprint = Some(fp);
        meta.version = CURRENT_VERSION - 1;
        assert!(!meta.is_current(&fp));
    }

    #[test]
    fn fingerprint_changes_when_the_file_does() {
        let dir = std::env::temp_dir().join(format!("gatefold-fp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("track.flac");
        fs::write(&file, b"abc").unwrap();
        let before = Fingerprint::of(&file).unwrap();
        assert_eq!(before.size, 3);
        fs::write(&file, b"abcd").unwrap();
        let after = Fingerprint::of(&file).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.size, 4);
        assert!(Fingerprint::of(&dir.join("missing.flac")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn album_artist_falls_back_to_track_artist() {
        let mut meta = TrackMetadata {
            version: CURRENT_VERSION,
            fingerprint: None,
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
            release: ReleaseTags::default(),
        };
        assert_eq!(meta.album_artist(), Some("Thom Yorke"));
        meta.album_artist_name = Some("Radiohead".into());
        assert_eq!(meta.album_artist(), Some("Radiohead"));
    }
}
