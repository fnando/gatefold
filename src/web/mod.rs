//! The web UI served by `gatefold run`: HTML pages (askama templates, htmx)
//! and a small JSON API under `/api/v1`. Built like tidepool's.
//!
//! Security: the `Host` header must name the listen address when it's a
//! loopback one (no DNS rebinding); with `server.token` set, every request
//! needs it (a bearer header, or the cookie the sign-in page sets); and every
//! POST must be same-origin: pages send the CSRF token htmx adds, the API
//! takes a bearer token or a JSON body. Files are only read or changed inside
//! the library folders.

mod config;
pub mod data;
mod library;
mod upload;

use crate::daemon::{Daemon, Options, Request as RunRequest};
use crate::db::{ChangeQuery, Db};
use crate::shutdown::Shutdown;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

const SESSION_COOKIE: &str = "gatefold_session";

/// What every request handler shares.
pub struct Web {
    daemon: Arc<Daemon>,
    token: String,
    csrf: String,
    /// `Host` values accepted; empty: any (a token protects the server).
    hosts: Vec<String>,
    /// Ends the live event streams when the server stops.
    shutdown: Shutdown,
}

impl Web {
    pub fn new(daemon: Arc<Daemon>, token: String, addr: SocketAddr, shutdown: Shutdown) -> Self {
        let port = addr.port();
        let hosts = if addr.ip().is_loopback() {
            vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]
        } else {
            Vec::new()
        };
        Self {
            daemon,
            token,
            csrf: random_hex(16),
            hosts,
            shutdown,
        }
    }

    /// What the sign-in cookie holds: derived from the token, so changing
    /// the token signs everyone out.
    fn session(&self) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(format!(
            "gatefold-session:{}",
            self.token
        )))
    }

    /// Run `f` with the daemon and its database, off the async threads.
    async fn with<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Daemon, &Db) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, WebError> {
        let daemon = self.daemon.clone();
        tokio::task::spawn_blocking(move || {
            let db = daemon.db()?;
            f(&daemon, &db)
        })
        .await
        .map_err(|e| WebError(anyhow::anyhow!(e)))?
        .map_err(WebError)
    }

    async fn status(self: &Arc<Self>) -> Result<data::Status, WebError> {
        self.with(data::status).await
    }
}

/// Serve `web` on `listener` until `shutdown` stops it.
pub async fn serve(
    web: Arc<Web>,
    listener: tokio::net::TcpListener,
    shutdown: Shutdown,
) -> std::io::Result<()> {
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move { shutdown.stopping().await })
        .await
}

pub fn router(web: Arc<Web>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/fragments/status", get(status_fragment))
        .route("/fragments/live", get(live_fragment))
        .route("/changes", get(changes_page))
        .route("/runs/{id}", get(run_page))
        .route("/actions/run", post(action_run))
        .route("/actions/stop", post(action_stop))
        .route("/actions/jobs/{name}/run", post(action_run_job))
        .route("/actions/schedule/{state}", post(action_schedule))
        .merge(library::routes())
        .merge(config::routes())
        .route("/login", get(login_page).post(login))
        .route("/assets/{file}", get(asset))
        .route("/api/v1/events", get(api_events))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/run", post(api_run))
        .layer(middleware::from_fn_with_state(web.clone(), guard))
        .layer(middleware::map_response(no_store))
        .with_state(web)
}

/// An error shown as a plain 500.
#[derive(Debug)]
pub struct WebError(anyhow::Error);

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        eprintln!("✗ web UI: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("something went wrong: {:#}", self.0),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for WebError {
    fn from(err: anyhow::Error) -> Self {
        Self(err)
    }
}

fn render(template: impl Template) -> Result<Html<String>, WebError> {
    template
        .render()
        .map(Html)
        .map_err(|e| WebError(anyhow::anyhow!(e)))
}

/// To `to`: an htmx redirect for htmx requests, else a plain one.
fn go_to(to: &str, headers: &HeaderMap) -> Response {
    if headers.contains_key("hx-request") {
        ([("hx-redirect", to.to_string())], StatusCode::OK).into_response()
    } else {
        Redirect::to(to).into_response()
    }
}

// ---- Dashboard -----------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    csrf: String,
    status: data::Status,
    message: Option<String>,
    live: crate::events::Live,
    version: &'static str,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusFragment {
    status: data::Status,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "live.html")]
struct LiveFragment {
    live: crate::events::Live,
}

async fn dashboard(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    render(DashboardPage {
        csrf: web.csrf.clone(),
        status: web.status().await?,
        message: None,
        live: crate::events::live(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn status_fragment(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    status_with(&web, None).await
}

async fn status_with(web: &Arc<Web>, message: Option<String>) -> Result<Html<String>, WebError> {
    render(StatusFragment {
        status: web.status().await?,
        message,
    })
}

async fn live_fragment() -> Result<Html<String>, WebError> {
    render(LiveFragment {
        live: crate::events::live(),
    })
}

/// The form behind "Run now" (and "Run on this folder").
#[derive(Debug, Default, Deserialize)]
struct RunForm {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    dry_run: Option<String>,
    #[serde(default)]
    replace_lyrics: Option<String>,
    #[serde(default)]
    replace_cover: Option<String>,
    #[serde(default)]
    replace_artist_images: Option<String>,
    #[serde(default)]
    only: Option<String>,
}

impl RunForm {
    fn options(&self) -> Options {
        let on = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.is_empty() && v != "0");
        Options {
            dry_run: on(&self.dry_run),
            replace_lyrics: on(&self.replace_lyrics),
            replace_cover: on(&self.replace_cover),
            replace_artist_images: on(&self.replace_artist_images),
            only: self
                .only
                .clone()
                .filter(|o| matches!(o.as_str(), "lyrics" | "cover" | "artist")),
        }
    }
}

/// Queue the run `form` asks for. A folder must be in the library.
fn queue_run(web: &Web, form: &RunForm) -> Result<String, Box<Response>> {
    let dir = match form.dir.as_deref().filter(|d| !d.is_empty()) {
        Some(dir) => Some(library::inside(&web.daemon.roots, dir).ok_or_else(|| {
            Box::new((StatusCode::NOT_FOUND, "not a folder in the library").into_response())
        })?),
        None => None,
    };
    Ok(web.daemon.request(RunRequest {
        trigger: "manual",
        job: None,
        dir,
        options: form.options(),
    }))
}

async fn action_run(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<RunForm>,
) -> Result<Response, WebError> {
    let message = match queue_run(&web, &form) {
        Ok(message) => message,
        Err(response) => return Ok(*response),
    };
    crate::events::emit(crate::events::Event::Changed);
    // From a folder page: back to it, saying so.
    if let Some(dir) = form
        .dir
        .as_deref()
        .filter(|d| !d.is_empty())
        .and_then(|d| library::inside(&web.daemon.roots, d))
    {
        return Ok(library::back_to(&dir, &message, &headers));
    }
    Ok(status_with(&web, Some(message)).await?.into_response())
}

#[derive(Debug, Default, Deserialize)]
struct StopForm {
    /// The lane to stop (a job, or `manual`); none: every run.
    #[serde(default)]
    lane: Option<String>,
}

async fn action_stop(
    State(web): State<Arc<Web>>,
    Form(form): Form<StopForm>,
) -> Result<Html<String>, WebError> {
    let lane = form.lane.filter(|l| !l.is_empty());
    let message = web.daemon.stop(lane.as_deref());
    crate::events::emit(crate::events::Event::Changed);
    status_with(&web, Some(message)).await
}

/// Run a job now (after its current run, if it's running).
async fn action_run_job(
    State(web): State<Arc<Web>>,
    Path(name): Path<String>,
) -> Result<Response, WebError> {
    let Some(message) = web.daemon.run_job(&name) else {
        return Ok((StatusCode::NOT_FOUND, "no such job").into_response());
    };
    crate::events::emit(crate::events::Event::Changed);
    Ok(status_with(&web, Some(message)).await?.into_response())
}

async fn action_schedule(
    State(web): State<Arc<Web>>,
    Path(state): Path<String>,
) -> Result<Response, WebError> {
    let paused = match state.as_str() {
        "pause" => true,
        "resume" => false,
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    web.with(move |daemon, _| daemon.set_paused(paused)).await?;
    crate::events::emit(crate::events::Event::Changed);
    let message = if paused {
        "Scheduled runs are paused. Runs you ask for still happen."
    } else {
        "Scheduled runs are back on."
    };
    Ok(status_with(&web, Some(message.to_string()))
        .await?
        .into_response())
}

// ---- Changes and runs ----------------------------------------------------

const PER_PAGE: usize = 100;

#[derive(Debug, Default, Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: Option<usize>,
}

#[derive(Template)]
#[template(path = "changes.html")]
struct ChangesPage {
    csrf: String,
    kind: String,
    query: String,
    groups: Vec<data::ChangeGroup>,
    chips: Vec<(&'static str, &'static str)>,
    page: usize,
    pages: usize,
    total: i64,
}

impl ChangesPage {
    fn link(&self, kind: &str, page: usize) -> String {
        let mut link = format!("/changes?kind={}", data::encode(kind));
        if !self.query.is_empty() {
            let _ = write!(link, "&q={}", data::encode(&self.query));
        }
        if page > 1 {
            let _ = write!(link, "&page={page}");
        }
        link
    }
}

async fn changes_page(
    State(web): State<Arc<Web>>,
    Query(params): Query<ChangesQuery>,
) -> Result<Html<String>, WebError> {
    let page = params.page.unwrap_or(1).max(1);
    let (kind, q) = (params.kind.clone(), params.q.clone());
    let (rows, total) = web
        .with(move |_, db| {
            let kinds = data::kinds_of(&kind);
            let query = ChangeQuery {
                artist: Some(q.as_str()).filter(|q| !q.is_empty()),
                kinds: &kinds,
                limit: PER_PAGE,
                offset: (page - 1) * PER_PAGE,
                ..ChangeQuery::default()
            };
            Ok((db.changes(&query)?, db.count_changes(&query)?))
        })
        .await?;
    let pages = usize::try_from(total)
        .unwrap_or(0)
        .div_ceil(PER_PAGE)
        .max(1);
    render(ChangesPage {
        csrf: web.csrf.clone(),
        kind: params.kind,
        query: params.q,
        groups: data::group(&rows),
        chips: data::GROUPS.to_vec(),
        page,
        pages,
        total,
    })
}

#[derive(Debug, Default, Deserialize)]
struct RunQuery {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    page: Option<usize>,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    csrf: String,
    run: data::RunView,
    stats: Vec<data::Stat>,
    groups: Vec<data::ChangeGroup>,
    lines: Vec<LineView>,
    kind: String,
    page: usize,
    pages: usize,
    total: i64,
    live: bool,
}

struct LineView {
    at: String,
    kind: String,
    text: String,
}

impl RunPage {
    fn link(&self, kind: &str, page: usize) -> String {
        let mut link = format!("/runs/{}?kind={kind}", self.run.id);
        if page > 1 {
            let _ = write!(link, "&page={page}");
        }
        link
    }
}

const LINES_PER_PAGE: usize = 300;

async fn run_page(
    State(web): State<Arc<Web>>,
    Path(id): Path<i64>,
    Query(params): Query<RunQuery>,
) -> Result<Response, WebError> {
    let page = params.page.unwrap_or(1).max(1);
    let kind = Some(params.kind.clone()).filter(|k| matches!(k.as_str(), "ok" | "info" | "error"));
    let found = web
        .with(move |_, db| {
            let Some(run) = db.run(id)? else {
                return Ok(None);
            };
            let changes = db.changes(&ChangeQuery {
                run_id: Some(id),
                limit: 500,
                ..ChangeQuery::default()
            })?;
            let lines = db.lines(
                id,
                kind.as_deref(),
                LINES_PER_PAGE,
                (page - 1) * LINES_PER_PAGE,
            )?;
            Ok(Some((run, changes, lines)))
        })
        .await?;
    let Some((run, changes, (lines, total))) = found else {
        return Ok((StatusCode::NOT_FOUND, "no such run").into_response());
    };
    let live = crate::events::live();
    let page_view = RunPage {
        csrf: web.csrf.clone(),
        stats: data::stats(run.summary_json.as_deref()),
        live: live.runs.iter().any(|r| r.run_id == Some(run.id)),
        run: data::RunView::from(&run),
        groups: data::group(&changes),
        lines: lines
            .into_iter()
            .map(|line| LineView {
                at: crate::changes::local_time(line.at)
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_string(),
                kind: line.kind,
                text: line.text,
            })
            .collect(),
        kind: params.kind,
        page,
        pages: usize::try_from(total)
            .unwrap_or(0)
            .div_ceil(LINES_PER_PAGE)
            .max(1),
        total,
    };
    Ok(render(page_view)?.into_response())
}

// ---- API -----------------------------------------------------------------

/// What happens as it happens, as server-sent events (JSON data, one
/// `Event` each). Ends when the server stops.
async fn api_events(State(web): State<Arc<Web>>) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;

    let Some(receiver) = crate::events::subscribe() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no live events here").into_response();
    };
    let shutdown = web.shutdown.clone();
    let stream = futures_util::stream::unfold(
        (receiver, shutdown),
        |(mut receiver, shutdown)| async move {
            let data = tokio::select! {
                () = shutdown.stopping() => return None,
                got = receiver.recv() => match got {
                    Ok(event) => serde_json::to_string(&event).expect("events serialize"),
                    // Too far behind: say something changed, the page reloads.
                    Err(RecvError::Lagged(_)) => r#"{"type":"changed"}"#.to_string(),
                    Err(RecvError::Closed) => return None,
                },
            };
            Some((
                Ok::<_, std::convert::Infallible>(SseEvent::default().data(data)),
                (receiver, shutdown),
            ))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn api_status(State(web): State<Arc<Web>>) -> Result<Response, WebError> {
    let status = web.status().await?;
    Ok(Json(serde_json::json!({
        "status": status,
        "live": crate::events::live(),
    }))
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
struct ApiRun {
    /// Run this job now (its options; `dir` and the rest are ignored).
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(flatten)]
    options: Options,
}

async fn api_run(State(web): State<Arc<Web>>, body: axum::body::Bytes) -> Response {
    let request: ApiRun = if body.is_empty() {
        ApiRun::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        }
    };
    if let Some(job) = request.job.as_deref() {
        return match web.daemon.run_job(job) {
            Some(message) => Json(serde_json::json!({ "message": message })).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "no such job" })),
            )
                .into_response(),
        };
    }
    let dir = match request.dir.as_deref() {
        Some(dir) => match library::inside(&web.daemon.roots, dir) {
            Some(dir) => Some(dir),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "not a folder in the library" })),
                )
                    .into_response();
            }
        },
        None => None,
    };
    let message = web.daemon.request(RunRequest {
        trigger: "manual",
        job: None,
        dir,
        options: request.options,
    });
    Json(serde_json::json!({ "message": message })).into_response()
}

// ---- Sign-in, assets and the guard ----------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    csrf: String,
    failed: bool,
}

async fn login_page(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    render(LoginPage {
        csrf: web.csrf.clone(),
        failed: false,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login(
    State(web): State<Arc<Web>>,
    Form(form): Form<LoginForm>,
) -> Result<Response, WebError> {
    if web.token.is_empty() {
        return Ok(Redirect::to("/").into_response());
    }
    if !same(&form.token, &web.token) {
        let page = render(LoginPage {
            csrf: web.csrf.clone(),
            failed: true,
        })?;
        return Ok((StatusCode::UNAUTHORIZED, page).into_response());
    }
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        web.session()
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("a valid cookie"),
    );
    Ok(response)
}

/// An asset built into the binary, and its content type.
fn asset_file(file: &str) -> Option<(&'static str, &'static str)> {
    Some(match file {
        "htmx.min.js" => (
            include_str!("../../assets/vendor/htmx.min.js"),
            "text/javascript",
        ),
        "app.css" => (include_str!("../../assets/app.css"), "text/css"),
        "live.js" => (include_str!("../../assets/live.js"), "text/javascript"),
        "toml.js" => (include_str!("../../assets/toml.js"), "text/javascript"),
        _ => return None,
    })
}

/// A short tag of an asset's contents.
fn asset_tag(body: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// An asset's address, which changes with its contents, so browsers never
/// use a copy from an older build.
pub(crate) fn asset_url(file: &str) -> String {
    let tag = asset_file(file).map_or_else(String::new, |(body, _)| asset_tag(body));
    format!("/assets/{file}?v={tag}")
}

/// An asset built into the binary. The browser checks back each time (a
/// tag of its contents), so a new build's assets show up right away.
async fn asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some((body, kind)) = asset_file(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", asset_tag(body));
    let cache = [
        (header::ETAG, etag.as_str()),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, kind)], body).into_response()
}

/// Pages, fragments and API answers are always fetched fresh, so a browser
/// never shows a page from an older build (or older state). Assets and
/// library files say their own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Host, origin, sign-in and CSRF checks, before any handler.
async fn guard(State(web): State<Arc<Web>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !web.hosts.is_empty() && !web.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    let path = request.uri().path().to_string();
    let api = path.starts_with("/api/");
    let post = request.method() == Method::POST;
    // A POST from another site's page.
    if post
        && let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok())
        && origin.split("://").nth(1) != Some(host.as_str())
    {
        return (StatusCode::FORBIDDEN, "cross-site request").into_response();
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|a| a.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| !web.token.is_empty() && same(t, &web.token));
    let open = path == "/login" || path.starts_with("/assets/");
    if !web.token.is_empty() && !open && !bearer && !signed_in(&web, headers) {
        return if api {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "sign in with a bearer token" })),
            )
                .into_response()
        } else if headers.contains_key("hx-request") {
            ([("hx-redirect", "/login")], StatusCode::UNAUTHORIZED).into_response()
        } else {
            Redirect::to("/login").into_response()
        };
    }
    if post && path != "/login" && !bearer {
        let csrf_ok = headers
            .get("x-csrf-token")
            .and_then(|t| t.to_str().ok())
            .is_some_and(|t| same(t, &web.csrf));
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("application/json"));
        if !(csrf_ok || (api && json)) {
            return (
                StatusCode::FORBIDDEN,
                if api {
                    "send Content-Type: application/json (or a bearer token)"
                } else {
                    "missing CSRF token; reload the page"
                },
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn signed_in(web: &Web, headers: &HeaderMap) -> bool {
    let session = web.session();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .any(|(name, value)| name == SESSION_COOKIE && same(value, &session))
}

/// Compare secrets in constant time.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom");
    hex::encode(&buf)
}

/// Where a path shows in the UI: relative to its library folder.
pub fn display_path(roots: &[PathBuf], path: &std::path::Path) -> String {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests;
