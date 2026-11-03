# gatefold

A command-line tool that scans a folder of music files and backfills missing
metadata — **lyrics**, **cover art**, and **artist images** — without overwriting anything
you already have.

- **Lyrics** come from [lrclib.net](https://lrclib.net): the plain lyrics are embedded in
  the file's lyrics tag, and synced lyrics are written to a `<track>.lrc` sidecar when the
  track doesn't already have one.
- **Cover art** comes from [Apple Music](https://music.apple.com) when the album can be
  found there (via its public lookup API, matched by barcode or `MusicBrainz` link; Apple's
  artwork is typically 3000–4000px), falling back to the
  [Cover Art Archive](https://coverartarchive.org) using the `MusicBrainz` ids in the tags.
  The front cover is embedded in the file and written once per album as `cover.jpg`.
- **Artist images** come from the artist's public [Apple Music](https://music.apple.com)
  page (square avatar + wide backdrop, plus the artist's motion backdrop as
  `backdrops/backdrop.mp4` when they have one and `ffmpeg` is installed, and their
  transparent logo as `logo.png` when they have one), falling back to
  [Spotify](https://open.spotify.com) (avatar only). The page URLs are looked up on
  `MusicBrainz` from the album-artist id in the tags, and the images are written to the
  artist folder as `folder.jpg`, `backdrop.jpg` and `logo.png` — the names Jellyfin reads.

## Features

- Recursively scans common audio formats (`mp3`, `flac`, `ogg`, `opus`, `m4a`, `mp4`,
  `wav`, `aac`, `wma`) via [`lofty`](https://crates.io/crates/lofty).
- Embeds plain lyrics into the unsynced-lyrics tag (ID3v2 `USLT`, Vorbis
  `UNSYNCEDLYRICS`, MP4 `©lyr`) and creates a synced `.lrc` sidecar when one is missing.
- Embeds the front cover and writes a per-folder `cover.jpg`. Apple Music's artwork is
  requested at up to 4000px; the Cover Art Archive fallback uses its ~1200px variant,
  resolved from the track's `MusicBrainz` release id (then its release group, where art
  often lives). Cover art is fetched once per release and shared across the album's tracks.
- Writes artist images once per album artist, into the folder named after the artist that
  sits above the album folders (`Artist/Album/track`). Apple's image host renders the
  renditions server-side, so the avatar comes at native resolution and the backdrop as a
  3840×2160 crop — no image processing happens locally.
- **Never overwrites** by default. With `--replace-lyrics` / `--replace-cover`, the
  corresponding embedded tag is overwritten and the previous value is snapshotted under
  `backup/` so it can be restored. Sidecars (`.lrc`, `cover.jpg`) are always create-only.
  Artist images are create-only too unless `--replace-artist-images` is set, which keeps the
  previous files, dated, in `<artist>/primary/` and `<artist>/backdrops/` before writing the
  new ones.
- **Keep your own images** with empty marker files: `.gatefold-keep` in an artist or album
  folder means its existing images are never replaced, even when replacing (missing ones are
  still written), and granular markers keep one kind each. See
  [Keeping your own images](#keeping-your-own-images).
- Optional `--verify`: each tag write is applied to a temp copy, validated with `ffprobe`,
  and only then atomically swapped over the original — a failed check never leaves a
  corrupted file.
- On-disk cache of API responses, downloaded art, and extracted tags, so re-runs avoid the
  network and re-parsing. "Not found" results are also recorded, and `--new-only` uses them to
  skip anything already looked up — so an incremental pass only touches brand-new files.
- `--cache-only` mode that populates the cache without touching any music file.
- Respects service rate limits: sequential requests with a configurable delay, and
  automatic `Retry-After` back-off (with jitter) on `429`/`503`, shown with a live
  countdown spinner.
- Sectioned, live logging with a progress bar and a continuously-refined ETA.

## Installation

```sh
cargo build --release
# binary at ./target/release/gatefold
```

## Usage

```sh
gatefold --state-dir <DIR> [OPTIONS] [PATH]
```

`PATH` is the folder to scan recursively (defaults to the current directory).

| Option | Description |
| --- | --- |
| `--state-dir <DIR>` | **Required.** Directory for the cached lyrics (`lrc/`), images (`art/`), artist lookups (`artist/`), and metadata (`metadata/`). |
| `--api-key <KEY>` | Secret `Lrclib-Api-Key` header (server-side model). Pair with `--app-id`. Prefer `--env-file` on shared hosts — a flag value shows up in `ps`. (env: `GATEFOLD_LRCLIB_API_KEY`) |
| `--app-id <ID>` | `Lrclib-App-Id` header. Required alongside `--api-key` to unlock the higher rate-limit tier. (env: `GATEFOLD_LRCLIB_APP_ID`) |
| `--env-file <FILE>` | Load `GATEFOLD_LRCLIB_API_KEY` / `GATEFOLD_LRCLIB_APP_ID` / `GATEFOLD_USER_AGENT` / `GATEFOLD_MUSICBRAINZ_URL` from a `KEY=VALUE` env file instead of passing them as flags. Flags and real environment variables override the file; a read failure exits with an error. |
| `--user-agent <UA>` | Override the `User-Agent` sent to lrclib.net, the Cover Art Archive, and `MusicBrainz`. (env: `GATEFOLD_USER_AGENT`) |
| `--musicbrainz-url <URL>` | Base URL of the `MusicBrainz` web service (default `https://musicbrainz.org`). Point it at a local mirror to skip the public server's 1 request/second floor, which gatefold applies only to `musicbrainz.org`. (env: `GATEFOLD_MUSICBRAINZ_URL`) |
| `--delay-ms <MS>` | Delay between API requests in milliseconds (default `300`; docs recommend 200–500). |
| `--cache-only` | Populate the caches but make no changes to music files. |
| `--replace-lyrics` | Overwrite an existing embedded lyrics tag (backing up the previous value). |
| `--replace-cover` | Overwrite an existing embedded cover and `cover.jpg` sidecar (backing up the previous values), e.g. to upgrade to Apple Music's larger artwork. |
| `--replace-artist-images` | Overwrite an artist folder's `folder.jpg` / `backdrop.jpg` / motion backdrop / `logo.png` when the artist's image has changed; the previous files are kept, dated, in `primary/`, `backdrops/` and `logos/`. |
| `--artist-refresh-after <DURATION>` | With `--replace-artist-images`, don't re-check an artist's page if it was last checked within this long ago (`30d`, `12h`, `90m`, `45s`, `2w`). Missing images are still written. |
| `--verify` | Validate each tag write with `ffprobe` on a temp copy before atomically swapping it in. |
| `--new-only` | Only look up items with no result on record yet — fast incremental re-runs. |
| `--miss-only` | Only re-check items previously recorded as "not found" (conflicts with `--new-only`). |
| `--retry-missed-after <DURATION>` | Don't retry an item recorded as "not found" within this long ago (`7d`, `12h`, …). Applies to normal runs, `--miss-only`, and artist replace runs. |
| `--lyrics-only` | Process lyrics only; skip cover art (conflicts with `--cover-only`). |
| `--cover-only` | Process cover art only; skip lyrics (conflicts with `--lyrics-only`). |
| `--artist-images-only` | Process artist images only; skip lyrics and cover art. |
| `--skip-artist-images` | Skip the artist-images pass. `--lyrics-only` / `--cover-only` imply it. |
| `--skip-artist-videos` | Don't download motion backdrops (`backdrops/backdrop.mp4`); still images only. |

### Examples

Preview what would happen and warm the cache, without modifying any files:

```sh
gatefold --state-dir /tmp/gatefold --cache-only ~/Music
```

Apply lyrics, reusing any cached responses from a previous `--cache-only` run:

```sh
gatefold --state-dir /tmp/gatefold ~/Music
```

With a registered server-side application for the higher rate-limit tier:

```sh
gatefold --state-dir /tmp/gatefold \
       --app-id my-app-id --api-key my-secret-key \
       ~/Music
```

On a server, keep the secret out of `ps` by putting the credentials in an env file
and loading it with `--env-file`:

```sh
mkdir -p ~/.local/config/gatefold
cat > ~/.local/config/gatefold/env <<'EOF'
GATEFOLD_LRCLIB_APP_ID=my-app-id
GATEFOLD_LRCLIB_API_KEY=my-secret-key
EOF
chmod 600 ~/.local/config/gatefold/env

gatefold --state-dir ~/.local/share/gatefold --env-file ~/.local/config/gatefold/env ~/Music
```

## How it works

For each discovered track:

1. **Metadata** is read from the file (artist, title, album, duration, `MusicBrainz`
   release / release-group ids, and whether lyrics or cover art are already present) and
   cached at `<state-dir>/metadata/<sha256(path)>.json`. Cached metadata is reused on later
   runs to avoid re-parsing.
2. **Lyrics**: unless the track already has both a lyrics tag and an adjacent `.lrc`, the
   lyrics record is loaded from `<state-dir>/lrc/<sha256(path)>.json` if present, otherwise
   fetched from `GET /api/get`. Only successful (`200`) responses are cached; `404` is not.
   When applying (non-`--cache-only`), the plain lyrics are written to the lyrics tag (if the
   track has none) and the synced lyrics to `<track>.lrc` (if no sidecar exists).
3. **Cover art**: when the file has a `MusicBrainz` release id, the album is first looked
   for on Apple Music through the public iTunes Lookup API (`itunes.apple.com/lookup`, no
   key): by the `BARCODE` tag if present, else by the barcode or Apple Music link on the
   `MusicBrainz` release, else by an Apple Music link on any other release in the same
   release group (one browse request). A match yields the artwork at `…/4000x4000bb.jpg`
   (capped at Apple's source size), which is the current digital edition's art. Otherwise
   the front cover comes from `coverartarchive.org/release/{id}/front`, falling back to
   `release-group/{id}/front`. The image is cached at `<state-dir>/art/<sha256(release)>.*`
   — keyed by release, so it's downloaded **once per album** and reused for every track on
   it (a release with no art is remembered for the run and not re-requested); the Apple
   match is cached at `<state-dir>/release/<id>.json`. When applying, the front cover is
   embedded (if the file has none) and written once per folder as `cover.jpg`.

Then, once per distinct **album artist** seen during the scan (the `MusicBrainz` album-artist
id in the tags; "Various Artists" is skipped):

4. **Artist images**: the artist's URL relations are fetched from `MusicBrainz`
   (`/ws/2/artist/{id}?inc=url-rels`, cached at `<state-dir>/artist/<id>.mb.json`) to find
   its Apple Music and Spotify pages. The Apple Music page is fetched first: it carries the
   artist's photo as a resizable asset, from which a native-size square avatar
   (`…/4000x4000bb.jpg`) and a 3840×2160 centre-cropped backdrop (`…/3840x2160sr.jpg`) are
   requested — when Apple has a dedicated wide banner for the artist, that is used for the
   backdrop instead. Artists without a photo on Apple Music (the page shows an album cover
   in its place, which is detected and rejected) fall back to Spotify, which only exposes a
   640px avatar. Some artists also have a looping **motion backdrop** on Apple Music; it is
   served as HLS, so when `ffmpeg` is on `PATH` the best SDR H.264 rendition at or under
   1080p is remuxed (no re-encoding) into `backdrops/backdrop.mp4` — Jellyfin's theme-video
   folder, where it plays as the artist's background rather than being imported as a music
   video (`--skip-artist-videos` opts out). Some artists also have a **logo** (their
   wordmark on a transparent background), requested at its native size as a PNG. The
   resolved URLs are cached at `<state-dir>/artist/<id>.json` and the bytes under `art/`,
   keyed by URL. When applying, the files are written as `folder.jpg`, `backdrop.jpg` (and
   `backdrops/backdrop.mp4` and `logo.png`) into the **artist folder**: walking up from the track's folder, the
   nearest ancestor whose name matches the album artist (ignoring case and punctuation, so
   `AC/DC` matches `AC-DC`) and that holds no audio files itself — album folders are never
   used, even for self-titled albums, because `folder.jpg` there would be read as the album
   cover. Tracks with no such folder above them (flat layouts) are reported and skipped.

   These pages are not a supported API: they are plain page fetches and may change without
   notice. The official Apple Music API would be the stable route, but it requires a
   paid developer token.

### Replacing tags & backups

By default existing tags are left alone. With `--replace-lyrics` and/or `--replace-cover`,
the corresponding tag is overwritten when the fetched value differs from what's embedded.
Because lofty only rewrites metadata (never the audio), restoring a change needs only the
previous tag value, so before overwriting, that value is snapshotted to:

```
<state-dir>/backup/<sha256(path)>/<unix-seconds>/
    lyrics.txt            # previous unsynced lyrics (if the lyrics tag was replaced)
    cover.<ext>           # previous embedded front cover (if the cover was replaced)
    cover-sidecar.<ext>   # previous cover.jpg from the folder (if the sidecar was replaced)
    meta.json             # original file path + which files were saved
```

Snapshots are only written when a tag is actually replaced with a *different* value, and
they're versioned by timestamp, so repeated replace runs keep a history you can restore
from. `--replace-cover` also replaces the folder's `cover.jpg` (snapshotted under the first
track that triggers it); `.lrc` sidecars and a `folder.jpg` used as album cover are never
touched.

Artist images are the exception, since artists do refresh their photo to match a new album.
With `--replace-artist-images`, each artist's page is re-resolved and, when the image URL
differs from the one recorded at the last write, the existing files are moved aside before
the new ones are written — into the artist folder's own archive subfolders, dated:

```
<artist folder>/
    folder.jpg                            # current avatar
    backdrop.jpg                          # current backdrop
    logo.png                              # current logo (artists that have one)
    primary/folder-<YYYY-MM-DD>.jpg       # previous avatar(s)
    backdrops/backdrop.mp4                # current motion backdrop (Jellyfin theme video)
    backdrops/backdrop-<YYYY-MM-DD>.jpg   # previous backdrop(s)
    backdrops/backdrop-<YYYY-MM-DD>.mp4   # previous motion backdrop(s)
    logos/logo-<YYYY-MM-DD>.png           # previous logo(s)
```

Every replace run fetches each artist's page to learn the current image URLs, and that's
usually the whole cost of the run on an up-to-date library. `--artist-refresh-after 30d`
skips the page fetch for artists checked less than 30 days ago (the check time is recorded
whether or not the images changed), so a scheduled replace run only re-checks a slice of the
library each time. Images an artist folder is missing are still written from the cached
resolution, as on any run; the summary counts the skipped artists as `fresh`.

A second replace run on the same day appends `-2`, `-3`, … rather than overwriting. Every
artist file gatefold writes, current or archived, gets a `<file>.sha256` sidecar with its
digest, so a replace run compares digests instead of rereading images and videos, and a
replaced file whose contents are already in the archive (an artist switching back to an
earlier photo) is dropped rather than archived twice. Unchanged images are left alone, so a
replace run on an up-to-date library is a no-op apart from the page fetches. Jellyfin plays everything in `backdrops/` as the artist's theme
videos, so a previous motion backdrop kept there rotates with the current one; delete the
dated copies once you're happy with the new one.

### Keeping your own images

For artists and albums whose art you picked yourself (a small artist with no photo on Apple
Music, a better scan than the Cover Art Archive has), drop an empty marker file next to it.
gatefold then never replaces (or archives) those images, even with `--replace-artist-images`
/ `--replace-cover`. A kept image that's missing is still written, as on any run, so a marker
only matters to replace runs. Lyrics are never affected.

`.gatefold-keep` keeps everything its folder governs. Without it, the granular markers keep
one kind each:

| Marker | Where | Keeps |
| --- | --- | --- |
| `.gatefold-keep` | artist folder | `folder.jpg`, `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`, and the covers of every album below it |
| `.gatefold-keep` | album folder | the album's cover (embedded in each track + `cover.jpg`) |
| `.gatefold-primary-keep` | artist folder | `folder.jpg` (the image archived in `primary/` on replace) |
| `.gatefold-backdrop-keep` | artist folder | `backdrop.jpg` |
| `.gatefold-video-keep` | artist folder | `backdrops/backdrop.mp4` |
| `.gatefold-logo-keep` | artist folder | `logo.png` |
| `.gatefold-cover-keep` | album folder, or artist folder for all its albums | the album cover (embedded + `cover.jpg`) |

```
Music/
    Bad Cop Bad Cop/
        .gatefold-keep              # artist images and every album cover below: yours
        folder.jpg
        2017 - Warriors/
    Kemuri/
        .gatefold-primary-keep      # folder.jpg is yours; backdrop, video + logo from Apple Music
        folder.jpg
        backdrop.jpg
        1997 - Little Playmate/
            .gatefold-cover-keep    # this album's cover is yours
            cover.jpg
```

For covers, gatefold checks the album folder and every folder above it up to the scanned
root, so markers above the root don't count. An artist split across two roots follows each
folder's own markers. On a replace run where every image an artist or album has is kept,
gatefold skips the lookups and the summary counts it as `kept`.

### Incremental runs (`--new-only`)

Every lookup records a result: a hit is cached (`lrc/`, `art/`), and a "not found" writes a
small `.miss` marker next to it. A hit later clears any stale `.miss` for that item.

- A **normal run ignores `.miss` markers** and re-queries anything not cached as a hit — so
  tracks lrclib didn't have yet get rechecked (they may have been added since).
- A run with **`--new-only`** skips any item that already has a result on record (hit *or* miss),
  only looking up things that have never been checked. Use it for a quick pass after you add
  new music to a library you've already processed.
- A run with **`--miss-only`** does the opposite: it only re-checks items previously recorded
  as "not found", skipping brand-new and already-resolved ones. Handy for periodically
  retrying misses after lrclib/CAA have had time to add content. (Conflicts with `--new-only`.)

`--lyrics-only` / `--cover-only` / `--artist-images-only` restrict which *kind* of data is
processed and compose with the above — e.g. `--cover-only --miss-only` retries only the
cover-art misses. For artists, a miss (no Apple Music / Spotify link on `MusicBrainz`, or no
usable image) is recorded as `artist/<id>.miss`; `--miss-only` re-fetches the links for those.

Retrying every miss on every run adds up once the list of things lrclib or Apple Music
simply don't have settles. `--retry-missed-after 7d` leaves alone any miss recorded (or last
confirmed) less than a week ago, so each miss is retried about weekly no matter how often
the script runs. It applies wherever a miss would otherwise be rechecked: normal runs,
`--miss-only`, and `--replace-artist-images` (which rechecks artist misses). The marker's
own timestamp is the clock, and a retry that misses again refreshes it.

### Verifying writes

With `--verify`, every tag write is staged safely: the change is applied to a temp copy in
the same folder, validated with `ffprobe`, and only swapped over the original with an atomic
rename once it passes. If the check fails, the temp copy is discarded and the original is
left untouched (and the failure is reported for that track). Requires `ffprobe` (ffmpeg) on
`PATH`; gatefold exits early if it's missing when `--verify` is set.

### Client identification & rate limits

lrclib requires clients to identify themselves via a `User-Agent`; a descriptive default
is sent automatically and can be overridden with `--user-agent`. Registration is
optional — `--app-id` / `--api-key` map to the `Lrclib-App-Id` / `Lrclib-Api-Key`
headers and unlock higher per-application allowances. On `429`/`503` the tool honors the
server's `Retry-After` header (plus random jitter) before retrying. See the
[lrclib API documentation](https://lrclib.net/docs) for details.

The Cover Art Archive signals rate limiting with `503`; the tool backs off the same way,
but a bounded number of times, so a sustained outage can't stall the run.

`MusicBrainz` asks for a descriptive `User-Agent` (the default includes the project URL) and
allows one request per second; gatefold enforces that floor on top of `--delay-ms` whenever
`--musicbrainz-url` points at `musicbrainz.org`. A local mirror is not throttled. The Apple
Music and Spotify pages are fetched with the same `User-Agent` at the `--delay-ms` pace.
Apple's iTunes Lookup API allows roughly 20 calls per minute, so album lookups run at least
3 seconds apart (once per release, cached).

## License

MIT
