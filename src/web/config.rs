//! The config editor: `config.toml` as text (literal secrets masked),
//! checked before it's saved, with every saved version kept in `gatefold.db`.
//! Like tidepool's. Settings take effect when `gatefold run` restarts.

use super::{Web, WebError, go_to, render};
use crate::config::Config;
use crate::config_edit;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::sync::Arc;

pub fn routes() -> Router<Arc<Web>> {
    Router::new()
        .route("/config", get(config_page))
        .route("/config/check", post(config_check))
        .route("/config/save", post(config_save))
        .route("/config/revisions/{rev}", get(revision_page))
        .route("/config/rollback/{rev}", post(config_rollback))
}

/// The config file's path, when there's one to edit.
fn file() -> Option<std::path::PathBuf> {
    crate::config::loaded_path().map(std::path::Path::to_path_buf)
}

/// The file's text (empty when it isn't there yet).
fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

struct RevisionRow {
    rev: i64,
    created_at: String,
    from: &'static str,
}

fn source_label(source: &str) -> &'static str {
    match source {
        "web" => "Saved in the web UI",
        "rollback" => "Rolled back in the web UI",
        _ => "The file, as found",
    }
}

#[derive(Template)]
#[template(path = "config.html")]
struct ConfigPage {
    csrf: String,
    rev: i64,
    file: String,
    text: String,
    message: Option<String>,
    history: Vec<RevisionRow>,
}

#[derive(Debug, Default, Deserialize)]
struct SavedQuery {
    #[serde(default)]
    saved: Option<i64>,
}

async fn config_page(
    State(web): State<Arc<Web>>,
    Query(params): Query<SavedQuery>,
) -> Result<Response, WebError> {
    let Some(path) = file() else {
        return Ok((
            StatusCode::NOT_FOUND,
            "no config file: neither $XDG_CONFIG_HOME nor $HOME is set",
        )
            .into_response());
    };
    let shown = path.clone();
    let (rev, text, history) = web
        .with(move |_, db| {
            let text = read(&path);
            let rev = db.sync_revision(&text)?;
            let history = db
                .revisions(30)?
                .into_iter()
                .map(|r| RevisionRow {
                    rev: r.rev,
                    created_at: crate::changes::local_time(r.created_at),
                    from: source_label(&r.source),
                })
                .collect();
            Ok((rev, text, history))
        })
        .await?;
    let page = ConfigPage {
        csrf: web.csrf.clone(),
        rev,
        file: shown.display().to_string(),
        text: config_edit::mask(&text),
        message: params.saved.map(|rev| {
            format!("Saved as revision {rev}. Restart gatefold run for it to take effect.")
        }),
        history,
    };
    Ok(render(page)?.into_response())
}

#[derive(Debug, Deserialize)]
struct ConfigForm {
    text: String,
    base: i64,
}

#[derive(Template)]
#[template(path = "config_result.html")]
struct ConfigResult {
    rev: i64,
    errors: Option<String>,
    text_changed: bool,
    changes: Vec<String>,
    diff: String,
    saved: bool,
}

/// Check `form` against the file: its full text (secrets put back), and the
/// result to show.
fn compare(form: &ConfigForm, current: &str, rev: i64) -> (String, ConfigResult) {
    let text = config_edit::unmask(&form.text.replace("\r\n", "\n"), current);
    let mut result = ConfigResult {
        rev,
        errors: None,
        text_changed: text != current,
        changes: Vec::new(),
        diff: config_edit::text_diff(
            &config_edit::mask(current),
            &config_edit::mask(&text),
            &format!("revision {rev}"),
            "yours",
        ),
        saved: false,
    };
    if form.base != rev {
        result.errors = Some(format!(
            "The config changed (now revision {rev}) since you opened it; reload the page."
        ));
        return (text, result);
    }
    match Config::parse(&text) {
        Ok(new) => {
            let old = Config::parse(current).unwrap_or_default();
            result.changes = config_edit::changes(&old, &new);
        }
        Err(err) => result.errors = Some(format!("{err:#}")),
    }
    (text, result)
}

async fn config_check(
    State(web): State<Arc<Web>>,
    Form(form): Form<ConfigForm>,
) -> Result<Response, WebError> {
    let Some(path) = file() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let result = web
        .with(move |_, db| {
            let current = read(&path);
            let rev = db.sync_revision(&current)?;
            Ok(compare(&form, &current, rev).1)
        })
        .await?;
    Ok(render(result)?.into_response())
}

/// Write the config file (only you can read it: it may hold secrets).
fn write_file(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let work = path.with_extension("toml.gatefold-tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&work)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&work, path)?;
    Ok(())
}

async fn config_save(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<ConfigForm>,
) -> Result<Response, WebError> {
    let Some(path) = file() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let outcome = web
        .with(move |_, db| {
            let current = read(&path);
            let rev = db.sync_revision(&current)?;
            let (text, result) = compare(&form, &current, rev);
            if result.errors.is_some() || !result.text_changed {
                return Ok(Err(result));
            }
            write_file(&path, &text)?;
            Ok(Ok(db.add_revision("web", &text)?))
        })
        .await?;
    match outcome {
        Ok(rev) => Ok(go_to(&format!("/config?saved={rev}"), &headers)),
        Err(result) => Ok(render(result)?.into_response()),
    }
}

#[derive(Template)]
#[template(path = "revision.html")]
struct RevisionPage {
    csrf: String,
    rev: i64,
    current: i64,
    created_at: String,
    from: &'static str,
    text: String,
    changes: Vec<String>,
}

async fn revision_page(
    State(web): State<Arc<Web>>,
    Path(rev): Path<i64>,
) -> Result<Response, WebError> {
    let Some(path) = file() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let found = web
        .with(move |_, db| {
            let current_text = read(&path);
            let current = db.sync_revision(&current_text)?;
            Ok(db
                .revision(rev)?
                .map(|revision| (revision, current, current_text)))
        })
        .await?;
    let Some((revision, current, current_text)) = found else {
        return Ok((StatusCode::NOT_FOUND, "no such revision").into_response());
    };
    let changes = match (Config::parse(&current_text), Config::parse(&revision.text)) {
        (Ok(now), Ok(then)) => config_edit::changes(&now, &then),
        _ => Vec::new(),
    };
    let page = RevisionPage {
        csrf: web.csrf.clone(),
        rev: revision.rev,
        current,
        created_at: crate::changes::local_time(revision.created_at),
        from: source_label(&revision.source),
        text: config_edit::mask(&revision.text),
        changes,
    };
    Ok(render(page)?.into_response())
}

async fn config_rollback(
    State(web): State<Arc<Web>>,
    Path(rev): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let Some(path) = file() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let saved = web
        .with(move |_, db| {
            let Some(revision) = db.revision(rev)? else {
                return Ok(None);
            };
            Config::parse(&revision.text)?;
            write_file(&path, &revision.text)?;
            Ok(Some(db.add_revision("rollback", &revision.text)?))
        })
        .await?;
    Ok(match saved {
        Some(rev) => go_to(&format!("/config?saved={rev}"), &headers),
        None => (StatusCode::NOT_FOUND, "no such revision").into_response(),
    })
}
