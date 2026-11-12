# gatefold

A command-line tool that scans a folder of music files and backfills missing
metadata — **lyrics**, **cover art**, and **artist images** — without overwriting anything
you already have.

- **Lyrics** come from [lrclib.net](https://lrclib.net): the plain lyrics are embedded in
  the file's lyrics tag, and synced lyrics are written to a `<track>.lrc` sidecar when the
  track doesn't already have one.
- **Cover art** comes from [Apple Music](https://music.apple.com) when the album can be
  found there (via its public lookup API, matched by barcode or `MusicBrainz` link; requested
  at 1500px), falling back to the
  [Cover Art Archive](https://coverartarchive.org) using the `MusicBrainz` ids in the tags.
  The front cover is embedded in the file and written once per album as `cover.jpg`.
- **Artist images** come from the artist's public [Apple Music](https://music.apple.com)
  page (square avatar + wide backdrop, plus the artist's motion backdrop as
  `backdrops/backdrop.mp4` when they have one and `ffmpeg` is installed, and their
  transparent logo as `logo.png` when they have one), falling back to
  [Spotify](https://open.spotify.com) (avatar only). The page URLs are looked up on
  `MusicBrainz` from the album-artist id in the tags (with [Wikidata](https://www.wikidata.org)
  as a fallback when `MusicBrainz` has no Apple Music link), and the images are written to
  the artist folder as `folder.jpg`, `backdrop.jpg` and `logo.png` — the names Jellyfin reads.

## Features

- Recursively scans common audio formats (`mp3`, `flac`, `ogg`, `opus`, `m4a`, `mp4`,
  `wav`, `aac`, `wma`) via [`lofty`](https://crates.io/crates/lofty).
- Embeds plain lyrics into the unsynced-lyrics tag (ID3v2 `USLT`, Vorbis
  `UNSYNCEDLYRICS`, MP4 `©lyr`) and creates a synced `.lrc` sidecar when one is missing.
- Embeds the front cover and writes a per-folder `cover.jpg`. Apple Music's artwork is
  requested at up to 1500px (its originals are often 3000–4000px, several megabytes in every
  track for nothing visible; smaller ones come as they are); the Cover Art Archive fallback
  uses its ~1200px variant,
  resolved from the track's `MusicBrainz` release id (then its release group, where art
  often lives). Cover art is fetched once per release and shared across the album's tracks.
- Writes artist images once per album artist, into the folder named after the artist that
  sits above the album folders (`Artist/Album/track`). Apple's image host renders the
  renditions server-side from its originals, so the avatar comes at up to 1500×1500 and the
  backdrop at 1920×1080 (matching the motion backdrops) — nothing is resized locally. AFI's photo, for one, is 3648×3648 at 4.0 MB on Apple; at 1500×1500 it's 683 KB.
- **Optimizes every image losslessly** before writing it: JPEGs are repacked (progressive,
  optimized Huffman tables) and PNGs recompressed, typically 10–35% smaller with exactly the
  same pixels. `gatefold optimize --dir <folder>` does the same for the images already in a
  library. See [Optimizing images](#optimizing-images).
- **Never overwrites** by default. With `--replace-lyrics` / `--replace-cover`, the
  corresponding embedded tag is overwritten and the previous value is snapshotted under
  `backup/` so it can be restored. Sidecars (`.lrc`, `cover.jpg`) are always create-only.
  Artist images are create-only too unless `--replace-artist-images` is set, which keeps the
  previous files, dated, in the artist folder's hidden `.gatefold-archive/` before writing the
  new ones.
- **Keep your own images** with empty marker files: `.gatefold-keep` in an artist or album
  folder means its existing images are never replaced, even when replacing (missing ones are
  still written), and granular markers keep one kind each; `gatefold keep --file <image>` / `--dir <folder>`
  writes them for you. See [Keeping your own images](#keeping-your-own-images).
- **Skip an image you don't want** with `gatefold skip --file <image>`. A different one replaces it
  as soon as one is available, and those exact contents are never written again. See
  [Skipping an image](#skipping-an-image).
- Optional `--verify`: each tag write is applied to a temp copy, validated with `ffprobe`,
  and only then atomically swapped over the original — a failed check never leaves a
  corrupted file.
- On-disk cache of API responses, downloaded art, and extracted tags, so re-runs avoid the
  network and re-parsing. "Not found" results are also recorded, and `--new-only` uses them to
  skip anything already looked up — so an incremental pass only touches brand-new files.
- **Jellyfin, both ways**: `gatefold jellyfin push` makes Jellyfin show the artist folders'
  images (with `backdrop.jpg` first), and `gatefold jellyfin pull` brings the images only
  Jellyfin has into the folders. See [Jellyfin](#jellyfin).
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
gatefold [--dir <DIR>] [OPTIONS]
```

| Option | Description |
| --- | --- |
| `--dir <DIR>` | Folder to scan recursively (default: the current directory). |
| `--state-dir <DIR>` | Use a different directory for the cached lyrics (`lrc/`), images (`art/`), artist lookups (`artist/`), metadata (`metadata/`), and tag backups (`backup/`). Defaults to `$XDG_DATA_HOME/gatefold`, or `~/.local/share/gatefold` when `$XDG_DATA_HOME` is unset. |
| `--api-key <KEY>` | Secret `Lrclib-Api-Key` header (server-side model). Pair with `--app-id`. Prefer `--env-file` on shared hosts — a flag value shows up in `ps`. (env: `GATEFOLD_LRCLIB_API_KEY`) |
| `--app-id <ID>` | `Lrclib-App-Id` header. Required alongside `--api-key` to unlock the higher rate-limit tier. (env: `GATEFOLD_LRCLIB_APP_ID`) |
| `--env-file <FILE>` | Load `GATEFOLD_LRCLIB_API_KEY` / `GATEFOLD_LRCLIB_APP_ID` / `GATEFOLD_USER_AGENT` / `GATEFOLD_MUSICBRAINZ_URL` / `GATEFOLD_JELLYFIN_URL` / `GATEFOLD_JELLYFIN_API_KEY` from a `KEY=VALUE` env file instead of passing them as flags. Flags and real environment variables override the file; a read failure exits with an error. |
| `--user-agent <UA>` | Override the `User-Agent` sent to lrclib.net, the Cover Art Archive, `MusicBrainz`, and Wikidata. (env: `GATEFOLD_USER_AGENT`) |
| `--musicbrainz-url <URL>` | Base URL of the `MusicBrainz` web service (default `https://musicbrainz.org`). Point it at a local mirror to skip the public server's 1 request/second floor, which gatefold applies only to `musicbrainz.org`. (env: `GATEFOLD_MUSICBRAINZ_URL`) |
| `--delay-ms <MS>` | Delay between API requests in milliseconds (default `300`; docs recommend 200–500). |
| `--cache-only` | Populate the caches but make no changes to music files. |
| `--replace-lyrics` | Overwrite an existing embedded lyrics tag (backing up the previous value). |
| `--replace-cover` | Overwrite an existing embedded cover and `cover.jpg` sidecar (backing up the previous values), e.g. to upgrade to Apple Music's larger artwork. |
| `--replace-artist-images` | Overwrite an artist folder's `folder.jpg` / `backdrop.jpg` / motion backdrop / `logo.png` when the artist's image has changed; the previous files are kept, dated, in `.gatefold-archive/` (`primary/`, `backdrops/` and `logos/`). |
| `--artist-refresh-after <DURATION>` | With `--replace-artist-images`, don't re-check an artist's page if it was last checked within this long ago (`30d`, `12h`, `90m`, `45s`, `2w`). Missing images are still written. |
| `--apple-url <URL>` | Use this Apple Music artist page (e.g. `https://music.apple.com/ca/artist/dead-fish/95758598`) for the one album artist under `--dir`, when `MusicBrainz` and Wikidata have no Apple Music link (or the wrong one). It's remembered in the state directory, so later runs keep using it, and this run replaces the artist's images as `--replace-artist-images` would. |
| `--verify` | Validate each tag write with `ffprobe` on a temp copy before atomically swapping it in. |
| `--new-only` | Only look up items with no result on record yet — fast incremental re-runs. |
| `--miss-only` | Only re-check items previously recorded as "not found" (conflicts with `--new-only`). |
| `--retry-missed-after <DURATION>` | Don't retry an item recorded as "not found" within this long ago (`7d`, `12h`, …). Applies to normal runs, `--miss-only`, and artist replace runs. |
| `--lyrics-only` | Process lyrics only; skip cover art (conflicts with `--cover-only`). |
| `--cover-only` | Process cover art only; skip lyrics (conflicts with `--lyrics-only`). |
| `--artist-images-only` | Process artist images only; skip lyrics and cover art. Only needs each album folder's album artist, so it reads the tags of up to 3 tracks per folder (first, middle, last) and goes with the `MusicBrainz` album artist most of them carry. |
| `--skip-artist-images` | Skip the artist-images pass. `--lyrics-only` / `--cover-only` imply it. |
| `--skip-artist-videos` | Don't download motion backdrops (`backdrops/backdrop.mp4`); still images only. |
| `--jellyfin-url <URL>` | Jellyfin server to keep in line with the artist folders after the artist pass (see [Jellyfin](#jellyfin)). (env: `GATEFOLD_JELLYFIN_URL`) |
| `--jellyfin-api-key <KEY>` | Jellyfin API key (Dashboard → API Keys). Prefer `--env-file`. (env: `GATEFOLD_JELLYFIN_API_KEY`) |

### Examples

Preview what would happen and warm the cache, without modifying any files:

```sh
gatefold --cache-only --dir ~/Music
```

Apply lyrics, reusing any cached responses from a previous `--cache-only` run:

```sh
gatefold --dir ~/Music
```

With a registered server-side application for the higher rate-limit tier:

```sh
gatefold --app-id my-app-id --api-key my-secret-key --dir ~/Music
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

gatefold --env-file ~/.local/config/gatefold/env --dir ~/Music
```

## How it works

For each discovered track:

1. **Metadata** is read from the file (artist, title, album, duration, `MusicBrainz`
   release / release-group ids, and whether lyrics or cover art are already present) and
   cached at `<state-dir>/metadata/<sha256(path)>.json` along with the file's mtime and size.
   Cached metadata is reused on later runs while the mtime and size still match; a file that
   was re-tagged or re-uploaded since is re-read, so changes it needs (e.g. a cover that is no
   longer embedded) are picked up. The snapshot is refreshed after gatefold's own writes.
2. **Lyrics**: unless the track already has both a lyrics tag and an adjacent `.lrc`, the
   lyrics record is loaded from `<state-dir>/lrc/<sha256(path)>.json` if present, otherwise
   fetched from `GET /api/get`. Only successful (`200`) responses are cached; `404` is not.
   When applying (non-`--cache-only`), the plain lyrics are written to the lyrics tag (if the
   track has none) and the synced lyrics to `<track>.lrc` (if no sidecar exists).
3. **Cover art**: when the file has a `MusicBrainz` release id, the album is first looked
   for on Apple Music through the public iTunes Lookup API (`itunes.apple.com/lookup`, no
   key): by the `BARCODE` tag if present, else by the barcode or Apple Music link on the
   `MusicBrainz` release, else by an Apple Music link on any other release in the same
   release group (one browse request). A match yields the artwork at `…/1500x1500bb.jpg`
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
   its Apple Music and Spotify pages. When `MusicBrainz` has no Apple Music link, the
   artist's Wikidata item is asked for one (and for a Spotify id, if that's missing too):
   one query to the public SPARQL endpoint, by the Wikidata item `MusicBrainz` links to or
   else by `MusicBrainz` artist id, cached at `<state-dir>/artist/<id>.wikidata.json`
   whether or not it found anything. The Apple Music page is fetched first: it carries the
   artist's photo as a resizable asset, from which a square avatar of up to 1500px
   (`…/1500x1500bb.jpg`) is requested, and a 1920×1080 backdrop from the artwork Apple shows
   as the artist's wide header (often the photo itself), or the photo when there's none. The
   backdrop is cropped around the faces (`…/1920x1080vf.jpg`): a plain centre crop
   (`…sr.jpg`) cuts through them on many band photos. Artists without a photo on Apple Music (the page shows an album cover
   in its place, which is detected and rejected) fall back to Spotify, which only exposes a
   640px avatar. Some artists also have a looping **motion backdrop** on Apple Music; it is
   served as HLS, so when `ffmpeg` is on `PATH` the best SDR H.264 rendition at or under
   1080p is re-encoded with x264 (CRF 23, fitted into 1920x1080, audio dropped — about
   half Apple's size with no visible difference; the stream is copied as-is when ffmpeg
   lacks `libx264` or only an HDR rendition exists) into `backdrops/backdrop.mp4` — Jellyfin's theme-video
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
the new ones are written — into the artist folder's hidden `.gatefold-archive/`, dated:

```
<artist folder>/
    folder.jpg                                              # current avatar
    backdrop.jpg                                            # current backdrop
    logo.png                                                # current logo (artists that have one)
    backdrops/backdrop.mp4                                  # current motion backdrop (Jellyfin theme video)
    .gatefold-archive/primary/folder-<YYYY-MM-DD>.jpg       # previous avatar(s)
    .gatefold-archive/backdrops/backdrop-<YYYY-MM-DD>.jpg   # previous backdrop(s)
    .gatefold-archive/backdrops/backdrop-<YYYY-MM-DD>.mp4   # previous motion backdrop(s)
    .gatefold-archive/logos/logo-<YYYY-MM-DD>.png           # previous logo(s)
```

Jellyfin ignores anything whose name starts with a dot (`**/.*` in its built-in ignore list),
so archived files are never indexed, and old motion backdrops never play as theme videos.
Earlier versions archived into `primary/`, `backdrops/` and `logos/` directly in the artist
folder; the artist pass moves those dated files into `.gatefold-archive/` (leaving the current
`backdrops/backdrop.mp4` and any file of yours alone) and removes the old folders once empty.

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
replace run on an up-to-date library is a no-op apart from the page fetches.

### Keeping your own images

For artists and albums whose art you picked yourself (a small artist with no photo on Apple
Music, a better scan than the Cover Art Archive has), drop an empty marker file next to it.
gatefold then never replaces (or archives) those images, even with `--replace-artist-images`
/ `--replace-cover`. A kept image that's missing is still written, as on any run, so a marker
only matters to replace runs. Lyrics are never affected.

`gatefold keep` writes the markers for you: `--file <image>` writes the marker for that kind of
image, in the folder that governs it; `--dir <folder>` writes `.gatefold-keep`.
Keeping an image you had [skipped](#skipping-an-image) takes the skip back.

```sh
gatefold keep --file "Music/Kemuri/folder.jpg"                        # .gatefold-primary-keep
gatefold keep --file "Music/Kemuri/1997 - Little Playmate/cover.jpg"  # .gatefold-cover-keep in the album
gatefold keep --dir "Music/Bad Cop Bad Cop"                           # .gatefold-keep
```

`.gatefold-keep` keeps everything its folder governs. Without it, the granular markers keep
one kind each:

| Marker | Where | Keeps |
| --- | --- | --- |
| `.gatefold-keep` | artist folder | `folder.jpg`, `backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`, and the covers of every album below it |
| `.gatefold-keep` | album folder | the album's cover (embedded in each track + `cover.jpg`) |
| `.gatefold-primary-keep` | artist folder | `folder.jpg` (the image archived in `.gatefold-archive/primary/` on replace) |
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
folder's own markers. On a replace run where every cover an album has is kept, gatefold skips
the lookups and the summary counts it as `kept`.

Kept artist images are still checked on `--replace-artist-images` runs (unless
`--artist-refresh-after` says the artist was checked recently). When Apple Music has a
different image than the one you kept, gatefold leaves yours in place and saves Apple's,
dated, in `.gatefold-archive/`. Only images that aren't archived yet are saved, so the archive
doesn't grow on every run. If you change your mind later,
[`gatefold restore`](#restoring-an-archived-image) brings it in.

### Skipping an image

A keep marker freezes a folder. Sometimes you want the opposite: you don't want the current
image (a wrong photo, a bad crop, a low-res scan), but a better one would be welcome later.
Skip it:

```sh
gatefold skip --file "Music/Kemuri/folder.jpg" --file "Music/Kemuri/1997 - Little Playmate/cover.jpg"
```

It writes an empty `.gatefold-<sha256>-skip` marker next to each image, named after the
image's contents, and leaves the image where it is. On later runs:

- An existing file with skipped contents counts as missing. The next run replaces it with a
  different image as soon as one is available, without `--replace-*` and even under a keep
  marker. The old file is archived (in `.gatefold-archive/`) or, for covers, backed
  up and swapped in each track as well as in `cover.jpg`.
- A downloaded image with skipped contents is never written into that folder. Album covers
  count these as `rejected` in the summary. Artist images are saved, dated, in
  `.gatefold-archive/` instead, unless they're there already, so
  [`gatefold restore`](#restoring-an-archived-image) can bring them back.
- When the cached cover is the skipped one, it's looked up again once per run, so a new Apple
  Music or Cover Art Archive image is picked up. Artist photos are re-checked on
  `--replace-artist-images` runs, as usual.

You can skip `cover.jpg` / `cover.jpeg` / `cover.png` and the artist files (`folder.jpg`,
`backdrop.jpg`, `backdrops/backdrop.mp4`, `logo.png`). Delete the marker to undo.

### Archiving an image

To move an artist file out of Jellyfin's sight right away, rather than waiting for a run to
replace it, archive it:

```sh
gatefold archive --file "Music/Kemuri/backdrop.jpg" --file "Music/Kemuri/backdrops/backdrop.mp4"
```

This takes `folder.jpg`, `backdrop.jpg`, `backdrops/backdrop.mp4` and `logo.png`. Each file
is [skipped](#skipping-an-image), then moved, dated, into the artist folder's
`.gatefold-archive/`, as `--replace-artist-images` would. The next run writes a different
image as soon as one is available, but never these contents again. To bring the file back, use [`gatefold restore`](#restoring-an-archived-image).

### Restoring an archived image

To bring back a previous artist photo, backdrop, motion backdrop or logo, point
`gatefold restore --dir` at the artist folder. It lists what's in its `.gatefold-archive/`
(newest first, marking versions you skipped) and asks which one to restore:

```
$ gatefold restore --dir "Music/Kemuri"
Archived artist files in Music/Kemuri:

  Avatar (current: folder.jpg)
    1) folder-2026-03-01.jpg  412 KB
    2) folder-2026-01-31.jpg  380 KB

  Logo (current: logo.png)
    3) logo-2026-02-01.png  24 KB

Restore which? [1-3, empty to cancel]: 2
✓ restored folder-2026-01-31.jpg as folder.jpg
  folder.jpg skipped and archived as .gatefold-archive/primary/folder-2026-11-08.jpg
```

Use `--file` with an archived file to restore it without the menu:
`gatefold restore --file "Music/Kemuri/.gatefold-archive/primary/folder-2026-01-31.jpg"`.

The file being replaced is [skipped](#skipping-an-image) and then archived, so later runs don't
write it back. If the restored image had been skipped, its marker is removed. A
`--replace-artist-images` run still replaces the restored image if the artist's photo changes
upstream; pass `--keep` to also write its [keep marker](#keeping-your-own-images)
(`.gatefold-primary-keep` for an avatar, and so on), so it stays.

### Optimizing images

Every image gatefold downloads (covers, artist images, logos) is optimized losslessly before
it's cached and written, so the same bytes go into `cover.jpg`, every track's tag and the
artist folder:

- **JPEG**: the compressed image data is repacked as it is, without decoding it to pixels
  and re-encoding: optimized Huffman tables and a progressive scan order searched for the
  smallest result, via [mozjpeg](https://github.com/mozilla/mozjpeg) (like
  `jpegtran -optimize -progressive`). The Exif block (orientation) and the colour profile are
  kept; comments, XMP and Photoshop blocks are dropped. Each result is read back and checked
  against the original, and kept only if it holds the same image data.
- **PNG**: recompressed with [oxipng](https://github.com/oxipng/oxipng), keeping the
  colour-related chunks.

A result that isn't smaller is discarded, and unreadable files are written as downloaded.
Motion backdrops aren't images: they get the [1080p x264 re-encode](#how-it-works) instead,
which is visually the same but not lossless. Apple Music artwork typically comes out 9–16% smaller as
JPEG and around a third smaller as PNG.

For the images and motion backdrops already in a library, run `gatefold optimize`:

```sh
gatefold optimize --dir Music --dry-run   # what would be saved
gatefold optimize --dir Music             # do it
```

It goes through artist files (`folder.jpg`, `backdrop.jpg`, `logo.png`) and their
`.gatefold-archive/`, album covers (`cover.jpg`) and the front covers embedded in tracks; the
same cover embedded in every track of an album is optimized once.

Motion backdrops (`backdrops/backdrop.mp4` and the archived ones) still as Apple served them,
from before gatefold re-encoded them on download, get that same re-encode now. Ones already
re-encoded are recognised by the x264 signature in the file and left alone, so a backdrop is
never re-encoded twice; HDR ones (PQ or HLG, or deeper than 8 bits) are kept as they are,
like on download. This needs `ffmpeg` with `libx264`; without it, backdrops are skipped and
the summary says so. A dry run lists the backdrops it would re-encode without encoding them,
so their savings only show on the real run.

Files you [keep](#keeping-your-own-images) are left byte for byte as they are. A
[skipped](#skipping-an-image) image stays skipped: its marker is renamed after the new
contents. Pass `--verify` to check each track with `ffprobe` before its new tag replaces the
original. Running it again finds nothing more to do.

`gatefold optimize` never resizes: that would decode and re-encode the image. To bring
artist images and covers written at Apple's larger sizes down to today's, run with
`--replace-artist-images` and `--replace-cover`: Apple serves the smaller renditions, and
the previous files are archived (artist images) or backed up (covers) as on any replace.

Cached images from before this existed are optimized the next time they're used. Files that
gatefold wrote before then still count as the same image, so `--replace-*` runs don't
rewrite them just because they weren't optimized.

### Jellyfin

Jellyfin picks up `folder.jpg`, `logo.png` and `backdrop.jpg` from the artist folders, but
it keeps the images it already had first: a backdrop it got from a metadata provider (or an
upload) stays ahead of the folder's `backdrop.jpg`. With the server set
(`GATEFOLD_JELLYFIN_URL` and `GATEFOLD_JELLYFIN_API_KEY`, or `--jellyfin-url` /
`--jellyfin-api-key`), gatefold talks to Jellyfin's API in both directions:

```sh
# Bring the images only Jellyfin has into the artist folders.
gatefold jellyfin pull --dir ~/Music --dry-run
gatefold jellyfin pull --dir ~/Music

# Make Jellyfin show the folders' images, with backdrop.jpg first.
gatefold jellyfin push --dir ~/Music
```

`--dir` takes an artist folder or a library of them (repeat it for several). Artists are
matched by their `MusicBrainz` id, else by folder name.

- **`push`** makes `folder.*` Jellyfin's photo, `logo.*` its logo, and `backdrop.*` its
  first backdrop. Jellyfin's own older backdrops are kept, after ours: its reorder call swaps
  the image *files* (it would overwrite `backdrop.jpg`), so they're deleted and uploaded
  again instead, which appends them. A photo or logo Jellyfin stored itself is deleted so it
  shows the folder's. Everything deleted is saved first under the state dir's
  `backup/jellyfin/<item id>/`, and nothing in the artist folders is ever changed or
  deleted. When the library saves artwork into media folders, uploads would land in the
  artist folders, so backdrops are left as they are. A normal run with the server set pushes
  every artist after its artist pass.
- **`pull`** copies the images only Jellyfin has (not its copies of the folders' own files)
  into the artist folders. Each is written when the folder has no file of its kind, and
  archived, dated, in `.gatefold-archive/` otherwise, so [`restore`](#restoring-an-archived-image)
  can bring it in. Existing files are never replaced, skip markers are honoured, images are
  optimized like any download, and a second pull adds nothing. JPEG and PNG only: WebP and
  AVIF stay in Jellyfin. Jellyfin itself isn't changed; run `push` next.

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
