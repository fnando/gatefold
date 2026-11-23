//! Paths given on the command line, found on disk even when their names are
//! spelled in another Unicode form than the file system stores (a shell sends
//! `ã` composed; a Samba share may store it as `a` + a combining tilde, and
//! doesn't treat the two as the same name).

use anyhow::{Context, Result, bail};
use std::convert::Infallible;
use std::fs;
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// A folder name as Jellyfin and macOS would agree on it: composed (NFC), with
/// the private-use characters a Samba server stores for `" * : < > ? \ |`
/// mapped back, and `/` read as `:` (how macOS shows it).
pub fn normalize(name: &str) -> String {
    name.nfc()
        .map(|c| match c {
            '\u{F020}' => '"',
            '\u{F021}' => '*',
            '\u{F022}' | '/' => ':',
            '\u{F023}' => '<',
            '\u{F024}' => '>',
            '\u{F025}' => '?',
            '\u{F026}' => '\\',
            '\u{F027}' => '|',
            '\u{F028}' => ' ',
            '\u{F029}' => '.',
            c => c,
        })
        .collect()
}

/// `path`, with each name that doesn't exist as given swapped for the entry
/// next to it that [`normalize`]s the same. Left as given from the first
/// name with no such entry.
pub fn resolve(path: &Path) -> PathBuf {
    if fs::symlink_metadata(path).is_ok() {
        return path.to_path_buf();
    }
    let mut out = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            out.push(component);
            continue;
        };
        let given = out.join(name);
        if fs::symlink_metadata(&given).is_ok() {
            out = given;
            continue;
        }
        let wanted = normalize(&name.to_string_lossy());
        let parent = if out.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &out
        };
        let found = fs::read_dir(parent).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name())
                .find(|entry| normalize(&entry.to_string_lossy()) == wanted)
        });
        if let Some(entry) = found {
            out.push(entry);
        } else {
            out.push(name);
            out.extend(components);
            break;
        }
    }
    out
}

/// Fail unless `path` is a folder, saying what it is instead; `hint` (e.g.
/// "use --file for a file") is added when it's a file.
pub fn require_dir(path: &Path, hint: Option<&str>) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if path.exists() {
        let hint = hint.map_or_else(String::new, |hint| format!("; {hint}"));
        bail!("{} is a file, not a folder{hint}", path.display());
    }
    bail!("{} doesn't exist", path.display());
}

/// Fail unless `path` is a file, saying what it is instead; `hint` (e.g.
/// "use --dir for a folder") is added when it's a folder.
pub fn require_file(path: &Path, hint: Option<&str>) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    if path.exists() {
        let hint = hint.map_or_else(String::new, |hint| format!("; {hint}"));
        bail!("{} is a folder, not a file{hint}", path.display());
    }
    bail!("{} doesn't exist", path.display());
}

/// Write `target` through a hidden `.<name>.part` next to it, renamed into
/// place, so a reader never sees half a file.
pub fn write_atomic(target: &Path, data: &[u8]) -> Result<()> {
    let name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?;
    let work = target.with_file_name(format!(".{}.part", name.to_string_lossy()));
    fs::write(&work, data).with_context(|| format!("writing {}", work.display()))?;
    fs::rename(&work, target)
        .with_context(|| format!("moving {} to {}", work.display(), target.display()))
}

/// Clap value parser for a music path: [`resolve`]d, never refused (a path
/// that isn't there is reported by the command that reads it).
#[allow(clippy::unnecessary_wraps)]
pub fn parse(value: &str) -> Result<PathBuf, Infallible> {
    Ok(resolve(Path::new(value)))
}

/// The file `name` in `dir`, or the one whose name differs from it only in
/// case (`01. song.lrc` for `01. Song.lrc`, `Cover.JPG` for `cover.jpg`).
/// A library shared with macOS, which ignores case, often has those; on a
/// disk that doesn't, they'd otherwise count as missing.
pub fn find_ignoring_case(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.is_file() {
        return Some(exact);
    }
    let wanted = name.to_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase() == wanted)
                && path.is_file()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_names_stored_in_another_unicode_form() {
        let root = std::env::temp_dir().join(format!("gatefold-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let stored = root.join("Ratos de Pora\u{303}o").join("Brasil");
        fs::create_dir_all(&stored).unwrap();

        // Some file systems (APFS) match either form themselves; others don't.
        let found = resolve(&root.join("Ratos de Porão").join("Brasil"));
        assert!(found.is_dir());
        assert_eq!(found.file_name(), stored.file_name());
        assert_eq!(resolve(&stored), stored);
        let missing = resolve(&root.join("Ratos de Porão").join("Missing").join("x"));
        assert!(missing.ends_with("Missing/x"));
        assert!(missing.parent().unwrap().parent().unwrap().is_dir());
        assert_eq!(resolve(&root.join("Nope")), root.join("Nope"));

        let file = stored.join("logo.png");
        fs::write(&file, b"x").unwrap();
        let err = |r: Result<()>| r.unwrap_err().to_string();
        assert!(
            err(require_dir(&file, Some("use --file")))
                .ends_with("is a file, not a folder; use --file")
        );
        assert!(err(require_file(&stored, None)).ends_with("is a folder, not a file"));
        assert!(err(require_dir(&root.join("Nope"), None)).ends_with("doesn't exist"));
        assert!(require_dir(&stored, None).is_ok() && require_file(&file, None).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }
}
