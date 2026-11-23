//! Editing `config.toml` from the web UI: literal secrets are shown masked
//! (comments and layout kept), a masked value saved back means "keep the
//! current one", and a check lists the settings a new text changes.

use crate::config::{Config, env_name};
use similar::TextDiff;
use toml_edit::DocumentMut;

/// What a masked secret looks like.
pub const MASK: &str = "********";

/// The keys holding secrets.
const SECRETS: &[(&str, &str)] = &[
    ("lrclib", "api_key"),
    ("jellyfin", "api_key"),
    ("server", "token"),
];

fn secret_value<'a>(doc: &'a DocumentMut, (table, key): (&str, &str)) -> Option<&'a str> {
    doc.get(table)?.get(key)?.as_str()
}

/// `text` with every literal secret replaced by [`MASK`] (a `$NAME`
/// reference is shown as is). Text that isn't TOML is returned unchanged.
pub fn mask(text: &str) -> String {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.to_string();
    };
    for &(table, key) in SECRETS {
        let literal = secret_value(&doc, (table, key))
            .is_some_and(|value| !value.is_empty() && env_name(value).is_none());
        if literal && let Some(item) = doc.get_mut(table).and_then(|t| t.get_mut(key)) {
            let decor = item.as_value().map(|v| v.decor().clone());
            *item = toml_edit::value(MASK);
            if let (Some(decor), Some(value)) = (decor, item.as_value_mut()) {
                *value.decor_mut() = decor;
            }
        }
    }
    doc.to_string()
}

/// `text` (edited) with each secret still [`MASK`]ed put back from
/// `current` (the file as it is).
pub fn unmask(text: &str, current: &str) -> String {
    let (Ok(mut doc), Ok(old)) = (text.parse::<DocumentMut>(), current.parse::<DocumentMut>())
    else {
        return text.to_string();
    };
    for &(table, key) in SECRETS {
        if secret_value(&doc, (table, key)) != Some(MASK) {
            continue;
        }
        let original = secret_value(&old, (table, key))
            .unwrap_or_default()
            .to_string();
        if let Some(item) = doc.get_mut(table).and_then(|t| t.get_mut(key)) {
            let decor = item.as_value().map(|v| v.decor().clone());
            *item = toml_edit::value(original);
            if let (Some(decor), Some(value)) = (decor, item.as_value_mut()) {
                *value.decor_mut() = decor;
            }
        }
    }
    doc.to_string()
}

/// The settings `new` changes from `old`, one line each
/// (`general.write_nfo: false → true`); secrets only say they changed.
pub fn changes(old: &Config, new: &Config) -> Vec<String> {
    let flat = |config: &Config| {
        let mut out = Vec::new();
        if let Ok(value) = toml::Value::try_from(config) {
            flatten("", &value, &mut out);
        }
        out
    };
    let (old, new) = (flat(old), flat(new));
    let secret = |key: &str| SECRETS.iter().any(|(t, k)| key == format!("{t}.{k}"));
    let mut lines = Vec::new();
    for (key, value) in &new {
        let before = old.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        if before == Some(value) {
            continue;
        }
        if secret(key) {
            lines.push(format!("{key}: changed"));
        } else {
            let before = before.map_or_else(|| "(unset)".to_string(), ToString::to_string);
            lines.push(format!("{key}: {before} → {value}"));
        }
    }
    lines
}

fn flatten(prefix: &str, value: &toml::Value, out: &mut Vec<(String, String)>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, out);
            }
        }
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// A unified diff of two texts.
pub fn text_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str =
        "version = 1\n\n[lrclib]\napi_key = \"abc\" # mine\n\n[jellyfin]\napi_key = \"$JF_KEY\"\n";

    #[test]
    fn literal_secrets_are_masked_and_kept() {
        let shown = mask(TEXT);
        assert!(shown.contains("api_key = \"********\" # mine"), "{shown}");
        assert!(shown.contains("\"$JF_KEY\""), "references stay");
        assert!(!shown.contains("abc"));
        assert_eq!(
            unmask(&shown, TEXT),
            TEXT,
            "saving the masked text keeps the secret"
        );
        let edited = shown.replace("version = 1", "version = 1\n# note");
        assert!(unmask(&edited, TEXT).contains("api_key = \"abc\" # mine"));
        let replaced = shown.replace("\"********\"", "\"new\"");
        assert!(unmask(&replaced, TEXT).contains("api_key = \"new\""));
    }

    #[test]
    fn lists_what_changes() {
        let old = Config::parse(TEXT).unwrap();
        let new = Config::parse(
            &TEXT
                .replace("abc", "xyz")
                .replace("version = 1", "version = 1\n[general]\nwrite_nfo = true"),
        )
        .unwrap();
        let lines = changes(&old, &new);
        assert!(
            lines.contains(&"general.write_nfo: false → true".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"lrclib.api_key: changed".to_string()),
            "{lines:?}"
        );
        assert_eq!(changes(&old, &old), Vec::<String>::new());
        assert!(text_diff("a\n", "b\n", "old", "new").contains("-a\n+b"));
    }
}
