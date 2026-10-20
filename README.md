# gatefold

A command-line tool that scans a folder of music files and backfills missing
metadata — **lyrics** and **cover art** — without overwriting anything you already have.

- **Lyrics** come from [lrclib.net](https://lrclib.net): the plain lyrics are embedded in
  the file's lyrics tag, and synced lyrics are written to a `<track>.lrc` sidecar when the
  track doesn't already have one.
- **Cover art** comes from the [Cover Art Archive](https://coverartarchive.org): using the
  `MusicBrainz` ids already in the file's tags (no extra lookups), the front cover is
  embedded in the file and written once per album as `cover.jpg`.

## Features

- Recursively scans common audio formats (`mp3`, `flac`, `ogg`, `opus`, `m4a`, `mp4`,
  `wav`, `aac`, `wma`) via [`lofty`](https://crates.io/crates/lofty).
- Embeds plain lyrics into the unsynced-lyrics tag (ID3v2 `USLT`, Vorbis
  `UNSYNCEDLYRICS`, MP4 `©lyr`) and creates a synced `.lrc` sidecar when one is missing.
- Embeds the front cover and writes a per-folder `cover.jpg`, resolved from the track's
  `MusicBrainz` release id (falling back to its release group, where art often lives). The
  ~1200px cover variant is used. Cover art is fetched once per release and shared across
  the album's tracks.
- **Never overwrites** by default. With `--replace-lyrics` / `--replace-cover`, the
  corresponding embedded tag is overwritten and the previous value is snapshotted under
  `backup/` so it can be restored. Sidecars (`.lrc`, `cover.jpg`) are always create-only.
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
| `--state-dir <DIR>` | **Required.** Directory for the cached lyrics (`lrc/`), cover art (`art/`), and metadata (`metadata/`). |
| `--api-key <KEY>` | Secret `Lrclib-Api-Key` header (server-side model). Pair with `--app-id`. Prefer `--env-file` on shared hosts — a flag value shows up in `ps`. (env: `GATEFOLD_LRCLIB_API_KEY`) |
| `--app-id <ID>` | `Lrclib-App-Id` header. Required alongside `--api-key` to unlock the higher rate-limit tier. (env: `GATEFOLD_LRCLIB_APP_ID`) |
| `--env-file <FILE>` | Load `GATEFOLD_LRCLIB_API_KEY` / `GATEFOLD_LRCLIB_APP_ID` / `GATEFOLD_USER_AGENT` from a `KEY=VALUE` env file instead of passing them as flags. Flags and real environment variables override the file; a read failure exits with an error. |
| `--user-agent <UA>` | Override the `User-Agent` sent to lrclib.net and the Cover Art Archive. (env: `GATEFOLD_USER_AGENT`) |
| `--delay-ms <MS>` | Delay between API requests in milliseconds (default `300`; docs recommend 200–500). |
| `--cache-only` | Populate the caches but make no changes to music files. |
| `--replace-lyrics` | Overwrite an existing embedded lyrics tag (backing up the previous value). |
| `--replace-cover` | Overwrite an existing embedded cover (backing up the previous value). |
| `--verify` | Validate each tag write with `ffprobe` on a temp copy before atomically swapping it in. |
| `--new-only` | Only look up items with no result on record yet — fast incremental re-runs. |
| `--miss-only` | Only re-check items previously recorded as "not found" (conflicts with `--new-only`). |
| `--lyrics-only` | Process lyrics only; skip cover art (conflicts with `--cover-only`). |
| `--cover-only` | Process cover art only; skip lyrics (conflicts with `--lyrics-only`). |

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
3. **Cover art**: when the file has a `MusicBrainz` release id, the front cover is fetched
   from `coverartarchive.org/release/{id}/front`, falling back to
   `release-group/{id}/front`. The image is cached at `<state-dir>/art/<sha256(release)>.*`
   — keyed by release, so it's downloaded **once per album** and reused for every track on
   it (a release with no art is remembered for the run and not re-requested). When applying,
   the front cover is embedded (if the file has none) and written once per folder as
   `cover.jpg`.

### Replacing tags & backups

By default existing tags are left alone. With `--replace-lyrics` and/or `--replace-cover`,
the corresponding tag is overwritten when the fetched value differs from what's embedded.
Because lofty only rewrites metadata (never the audio), restoring a change needs only the
previous tag value, so before overwriting, that value is snapshotted to:

```
<state-dir>/backup/<sha256(path)>/<unix-seconds>/
    lyrics.txt     # previous unsynced lyrics (if the lyrics tag was replaced)
    cover.<ext>    # previous embedded front cover (if the cover was replaced)
    meta.json      # original file path + which files were saved
```

Snapshots are only written when a tag is actually replaced with a *different* value, and
they're versioned by timestamp, so repeated replace runs keep a history you can restore
from. Sidecar files (`.lrc`, `cover.jpg`) are never replaced, so they're never backed up.

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

`--lyrics-only` / `--cover-only` restrict which *kind* of data is processed and compose with
the above — e.g. `--cover-only --miss-only` retries only the cover-art misses.

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

## License

MIT
