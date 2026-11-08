use crate::state::digest_bytes;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sidecar next to each artist file (current and archived) holding its SHA-256
/// hex digest, so later runs can compare files without rereading them.
const DIGEST_EXT: &str = "sha256";

/// What happened to a file that was moved aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archived {
    /// Moved into the archive under this path.
    Moved(PathBuf),
    /// An identical file was already archived at this path; the duplicate was removed.
    Duplicate(PathBuf),
}

impl Archived {
    pub fn path(&self) -> &Path {
        match self {
            Archived::Moved(p) | Archived::Duplicate(p) => p,
        }
    }
}

/// Where an artist folder's previous files go when `--replace-artist-images`
/// overwrites them: an archive folder under the artist folder's hidden
/// `.gatefold-archive/` (`primary/` for avatars, `backdrops/` for still and
/// motion backdrops, `logos/` for logos), with the run's date in the name so
/// repeated replace runs keep a history.
pub struct Revisions {
    stamp: u64,
}

impl Revisions {
    pub fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self { stamp }
    }

    /// Unix seconds at which this run started.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// This run's date, `YYYY-MM-DD` (UTC).
    pub fn date(&self) -> String {
        let (year, month, day, ..) = civil_from_unix(self.stamp);
        format!("{year:04}-{month:02}-{day:02}")
    }

    /// Move `previous` (whose SHA-256 is `digest`) into `archive_dir` as
    /// `<stem>-<date>.<ext>`, appending `-2`, `-3`, … when that name is already
    /// taken (a second replace run on the same day). When the archive already
    /// holds an identical file, `previous` is removed instead of copied again.
    /// The file's digest sidecar follows it (or is removed with it).
    pub fn archive(&self, archive_dir: &Path, previous: &Path, digest: &str) -> Result<Archived> {
        fs::create_dir_all(archive_dir)
            .with_context(|| format!("creating {}", archive_dir.display()))?;

        if let Some(existing) = find_archived(archive_dir, digest)? {
            fs::remove_file(previous)
                .with_context(|| format!("removing {}", previous.display()))?;
            let _ = fs::remove_file(digest_path(previous));
            return Ok(Archived::Duplicate(existing));
        }

        let stem = previous
            .file_stem()
            .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());
        let ext = previous
            .extension()
            .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
        let date = self.date();
        let mut target = archive_dir.join(format!("{stem}-{date}{ext}"));
        let mut n = 1;
        while target.exists() {
            n += 1;
            target = archive_dir.join(format!("{stem}-{date}-{n}{ext}"));
        }
        fs::rename(previous, &target)
            .with_context(|| format!("moving {} to {}", previous.display(), target.display()))?;
        let _ = fs::remove_file(digest_path(previous));
        write_digest(&target, digest)?;
        Ok(Archived::Moved(target))
    }
}

/// True when `path` is named like a file [`Revisions::archive`] wrote for `stem`:
/// `<stem>-YYYY-MM-DD.<ext>` or `<stem>-YYYY-MM-DD-<n>.<ext>`, not a sidecar.
pub fn is_archived_name(path: &Path, stem: &str) -> bool {
    if path.extension().is_some_and(|e| e == DIGEST_EXT) {
        return false;
    }
    let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(rest) = name.strip_prefix(stem).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = rest.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => digits(y, 4) && digits(m, 2) && digits(d, 2),
        [y, m, d, n] => {
            digits(y, 4) && digits(m, 2) && digits(d, 2) && !n.is_empty() && digits(n, n.len())
        }
        _ => false,
    }
}

/// Move the archived `file` (and its digest sidecar) into `archive_dir`, keeping
/// its name: appending `-2`, `-3`, … when another file has it, or dropping
/// `file` when the archive already holds the same contents.
pub fn relocate(file: &Path, archive_dir: &Path) -> Result<()> {
    fs::create_dir_all(archive_dir)
        .with_context(|| format!("creating {}", archive_dir.display()))?;
    let digest = file_digest(file)?;
    if find_archived(archive_dir, &digest)?.is_some() {
        fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?;
        let _ = fs::remove_file(digest_path(file));
        return Ok(());
    }
    let name = file.file_name().unwrap_or_default();
    let mut target = archive_dir.join(name);
    let stem = file
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let ext = file
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let mut n = 1;
    while target.exists() {
        n += 1;
        target = archive_dir.join(format!("{stem}-{n}{ext}"));
    }
    fs::rename(file, &target)
        .with_context(|| format!("moving {} to {}", file.display(), target.display()))?;
    let _ = fs::remove_file(digest_path(file));
    write_digest(&target, &digest)
}

/// Move `from` (whose SHA-256 is `digest`) to `to`, its digest sidecar with it.
pub fn move_file(from: &Path, to: &Path, digest: &str) -> Result<()> {
    fs::rename(from, to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    let _ = fs::remove_file(digest_path(from));
    write_digest(to, digest)
}

/// An archived file with the given digest, if any. Each file's digest comes from
/// its `.sha256` sidecar, computed (and written) on first sight.
fn find_archived(archive_dir: &Path, digest: &str) -> Result<Option<PathBuf>> {
    let entries =
        fs::read_dir(archive_dir).with_context(|| format!("reading {}", archive_dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if !path.is_file() || path.extension().is_some_and(|e| e == DIGEST_EXT) {
            continue;
        }
        if file_digest(&path)? == digest {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn digest_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".");
    name.push(DIGEST_EXT);
    PathBuf::from(name)
}

/// The SHA-256 of `file`, from its sidecar when present and well-formed, else
/// computed from the file and written to the sidecar for next time.
pub fn file_digest(file: &Path) -> Result<String> {
    let sidecar = digest_path(file);
    if let Ok(stored) = fs::read_to_string(&sidecar) {
        let stored = stored.trim();
        if stored.len() == 64 && stored.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(stored.to_string());
        }
    }
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let digest = digest_bytes(&bytes);
    write_digest(file, &digest)?;
    Ok(digest)
}

/// Write (or refresh) the digest sidecar next to `file`.
pub fn write_digest(file: &Path, digest: &str) -> Result<()> {
    let sidecar = digest_path(file);
    fs::write(&sidecar, format!("{digest}\n"))
        .with_context(|| format!("writing {}", sidecar.display()))
}

/// Civil date and time (UTC) for a unix timestamp, without a date crate.
/// Days-to-civil is Howard Hinnant's algorithm.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
pub fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_unix_time_to_civil() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-11-02T11:00:00Z
        assert_eq!(civil_from_unix(1_793_617_200), (2026, 11, 2, 11, 0, 0));
        // 2000-02-29T23:59:59Z (leap day)
        assert_eq!(civil_from_unix(951_868_799), (2000, 2, 29, 23, 59, 59));
    }

    #[test]
    fn recognizes_archived_names() {
        for name in [
            "folder-2026-01-31.jpg",
            "folder-2026-01-31-2.jpg",
            "folder-2026-01-31-12.png",
        ] {
            assert!(is_archived_name(Path::new(name), "folder"), "{name}");
        }
        for name in [
            "folder.jpg",
            "folder-2026-01-31.jpg.sha256",
            "backdrop-2026-01-31.jpg",
            "folder-26-01-31.jpg",
            "folder-2026-01-31-.jpg",
            "folder-2026-01-31-x.jpg",
            "folder-cover.jpg",
        ] {
            assert!(!is_archived_name(Path::new(name), "folder"), "{name}");
        }
    }

    #[test]
    fn archives_with_dated_unique_names_and_skips_duplicates() {
        let root = std::env::temp_dir().join(format!("gatefold-rev-{}", std::process::id()));
        let artist = root.join("Artist");
        fs::create_dir_all(&artist).unwrap();
        let revisions = Revisions::new();
        let date = revisions.date();
        let archive = artist.join("primary");
        let current = artist.join("folder.jpg");

        fs::write(&current, b"one").unwrap();
        let first = revisions
            .archive(&archive, &current, &digest_bytes(b"one"))
            .unwrap();
        let first_path = archive.join(format!("folder-{date}.jpg"));
        assert_eq!(first, Archived::Moved(first_path.clone()));
        assert!(!current.exists());
        assert_eq!(fs::read(&first_path).unwrap(), b"one");
        assert_eq!(
            fs::read_to_string(archive.join(format!("folder-{date}.jpg.sha256")))
                .unwrap()
                .trim(),
            digest_bytes(b"one")
        );

        fs::write(&current, b"two").unwrap();
        write_digest(&current, &digest_bytes(b"two")).unwrap();
        let second = revisions
            .archive(&archive, &current, &digest_bytes(b"two"))
            .unwrap();
        assert!(!digest_path(&current).exists(), "sidecar follows the file");
        assert_eq!(
            second,
            Archived::Moved(archive.join(format!("folder-{date}-2.jpg")))
        );

        // The same bytes again: nothing new is archived, the duplicate is removed.
        fs::write(&current, b"one").unwrap();
        let again = revisions
            .archive(&archive, &current, &digest_bytes(b"one"))
            .unwrap();
        assert_eq!(again, Archived::Duplicate(first_path));
        assert!(!current.exists());
        assert_eq!(fs::read_dir(&archive).unwrap().count(), 4); // 2 files + 2 sidecars

        // A pre-existing archive file without a sidecar gets hashed and tagged once.
        fs::write(archive.join("folder-2020-01-01.jpg"), b"old").unwrap();
        fs::write(&current, b"old").unwrap();
        let old = revisions
            .archive(&archive, &current, &digest_bytes(b"old"))
            .unwrap();
        assert_eq!(
            old,
            Archived::Duplicate(archive.join("folder-2020-01-01.jpg"))
        );
        assert!(archive.join("folder-2020-01-01.jpg.sha256").is_file());

        fs::remove_dir_all(&root).unwrap();
    }
}
