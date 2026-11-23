use super::*;
use axum::body::Body;
use clap::Parser;
use tower::ServiceExt;

/// A library with one artist (a logo, an album with a cover) and a state dir.
struct Setup {
    root: PathBuf,
    web: Arc<Web>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn setup(name: &str, token: &str) -> Setup {
    setup_with_jobs(name, token, Vec::new())
}

fn setup_with_jobs(name: &str, token: &str, jobs: Vec<crate::daemon::JobLane>) -> Setup {
    let root = std::env::temp_dir().join(format!("gatefold-web-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let library = root.join("music");
    let album = library.join("AFI").join("Sing the Sorrow");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(library.join("AFI").join("logo.png"), b"\x89PNGlogo").unwrap();
    std::fs::write(album.join("cover.jpg"), b"\xff\xd8cover").unwrap();
    std::fs::write(album.join("01. Miseria Cantare.flac"), b"not really flac").unwrap();
    let library = std::fs::canonicalize(&library).unwrap();
    let cli = crate::cli::Cli::parse_from(["gatefold"]);
    let daemon = Arc::new(Daemon::new(cli, root.join("state"), vec![library], jobs));
    let addr: SocketAddr = "127.0.0.1:8788".parse().unwrap();
    let web = Arc::new(Web::new(
        daemon,
        token.to_string(),
        addr,
        Shutdown::default(),
    ));
    Setup { root, web }
}

impl Setup {
    fn library(&self) -> PathBuf {
        self.web.daemon.roots[0].clone()
    }
}

async fn send(
    web: &Arc<Web>,
    mut request: axum::http::Request<Body>,
) -> (StatusCode, HeaderMap, String) {
    if !request.headers().contains_key(header::HOST) {
        request
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static("127.0.0.1:8788"));
    }
    let response = router(web.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(uri: &str) -> axum::http::Request<Body> {
    axum::http::Request::get(uri).body(Body::empty()).unwrap()
}

fn form(web: &Web, uri: &str, body: &str) -> axum::http::Request<Body> {
    axum::http::Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-csrf-token", web.csrf.clone())
        .header("hx-request", "true")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn the_dashboard_and_pages_render() {
    let s = setup("pages", "");
    let (status, _, body) = send(&s.web, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Run everything now"), "{body}");
    assert!(body.contains("Nothing changed yet."));
    for uri in [
        "/changes",
        "/library",
        "/fragments/status",
        "/fragments/live",
    ] {
        let (status, _, _) = send(&s.web, get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (_, _, library) = send(&s.web, get("/library")).await;
    assert!(library.contains("AFI"));
    let artist = s.library().join("AFI");
    let (status, _, page) = send(
        &s.web,
        get(&data::artist_link(&artist.display().to_string())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("Logo") && page.contains("Sing the Sorrow"),
        "{page}"
    );
    let (status, _, page) = send(
        &s.web,
        get(&data::album_link(&artist.join("Sing the Sorrow"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Animated cover"));
    let (status, _, _) = send(&s.web, get("/runs/99")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nothing_outside_the_library() {
    let s = setup("outside", "");
    let outside = s.root.join("state");
    for uri in [
        format!(
            "/artist?dir={}",
            data::encode(&outside.display().to_string())
        ),
        format!("/file?path={}", data::encode("/etc/hosts")),
        format!(
            "/artist?dir={}",
            data::encode(&format!("{}/../state", s.library().display()))
        ),
    ] {
        let (status, _, _) = send(&s.web, get(&uri)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // Audio isn't served, images are.
    let logo = s.library().join("AFI/logo.png");
    let (status, headers, _) = send(
        &s.web,
        get(&format!(
            "/file?path={}",
            data::encode(&logo.display().to_string())
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
}

#[tokio::test]
async fn marker_actions_write_and_remove_markers_and_are_recorded() {
    let s = setup("markers", "");
    let logo = s.library().join("AFI/logo.png");
    let path = format!("path={}", data::encode(&logo.display().to_string()));

    let (status, headers, _) = send(&s.web, form(&s.web, "/actions/file/keep", &path)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["hx-redirect"]
            .to_str()
            .unwrap()
            .starts_with("/artist?dir=")
    );
    assert!(s.library().join("AFI/.gatefold-logo-keep").is_file());

    send(&s.web, form(&s.web, "/actions/file/unkeep", &path)).await;
    assert!(!s.library().join("AFI/.gatefold-logo-keep").exists());

    send(&s.web, form(&s.web, "/actions/file/skip", &path)).await;
    assert!(crate::skip::rejected(&logo));
    send(&s.web, form(&s.web, "/actions/file/unskip", &path)).await;
    assert!(!crate::skip::rejected(&logo));

    let album = s.library().join("AFI/Sing the Sorrow");
    let dir = format!("dir={}", data::encode(&album.display().to_string()));
    send(&s.web, form(&s.web, "/actions/folder/keep", &dir)).await;
    assert!(album.join(".gatefold-keep").is_file());

    let (_, _, changes) = send(&s.web, get("/changes")).await;
    for text in ["Kept", "Stopped keeping", "Skipped", "Took back a skip"] {
        assert!(changes.contains(text), "{text}");
    }
    let artist = s.library().join("AFI");
    let (_, _, page) = send(
        &s.web,
        get(&data::artist_link(&artist.display().to_string())),
    )
    .await;
    assert!(page.contains("Took back a skip"), "the artist's history");

    // Only files gatefold manages.
    let cover = form(&s.web, "/actions/file/keep", "path=/etc/hosts");
    assert_eq!(send(&s.web, cover).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn posts_need_the_csrf_token_and_same_origin() {
    let s = setup("csrf", "");
    let request = axum::http::Request::post("/actions/run")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&s.web, request).await.0, StatusCode::FORBIDDEN);
    let mut request = form(&s.web, "/actions/run", "");
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("http://evil.example"),
    );
    assert_eq!(send(&s.web, request).await.0, StatusCode::FORBIDDEN);
    let mut request = get("/");
    request
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("evil.example"));
    assert_eq!(
        send(&s.web, request).await.0,
        StatusCode::MISDIRECTED_REQUEST
    );

    let (status, _, body) = send(&s.web, form(&s.web, "/actions/run", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Starting a run."), "{body}");
    assert_eq!(s.web.daemon.queued(), 1);
}

#[tokio::test]
async fn a_token_guards_everything_but_sign_in_and_assets() {
    let s = setup("token", "s3cret");
    let (status, headers, _) = send(&s.web, get("/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/login");
    assert_eq!(send(&s.web, get("/assets/app.css")).await.0, StatusCode::OK);
    assert_eq!(
        send(&s.web, get("/api/v1/status")).await.0,
        StatusCode::UNAUTHORIZED
    );

    let wrong = axum::http::Request::post("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("token=nope"))
        .unwrap();
    assert_eq!(send(&s.web, wrong).await.0, StatusCode::UNAUTHORIZED);
    let right = axum::http::Request::post("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("token=s3cret"))
        .unwrap();
    let (_, headers, _) = send(&s.web, right).await;
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    let session = cookie.split(';').next().unwrap().to_string();
    let mut signed_in = get("/");
    signed_in
        .headers_mut()
        .insert(header::COOKIE, HeaderValue::from_str(&session).unwrap());
    assert_eq!(send(&s.web, signed_in).await.0, StatusCode::OK);
    let mut bearer = get("/api/v1/status");
    bearer.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer s3cret"),
    );
    assert_eq!(send(&s.web, bearer).await.0, StatusCode::OK);
}

#[tokio::test]
async fn assets_are_cached_by_their_tag() {
    let s = setup("assets", "");
    let (status, headers, _) = send(&s.web, get("/assets/app.css")).await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers[header::ETAG].clone();
    let mut again = get("/assets/app.css");
    again.headers_mut().insert(header::IF_NONE_MATCH, etag);
    assert_eq!(send(&s.web, again).await.0, StatusCode::NOT_MODIFIED);
    assert_eq!(
        send(&s.web, get("/assets/nope.js")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn file_names_in_the_wrong_case_are_fixed() {
    let s = setup("case", "");
    let album = s.library().join("AFI/Sing the Sorrow");
    std::fs::write(album.join("01. miseria cantare.LRC"), b"[00:01.00]x").unwrap();
    let artist = s.library().join("AFI");
    let (_, _, page) = send(
        &s.web,
        get(&data::artist_link(&artist.display().to_string())),
    )
    .await;
    assert!(page.contains("should be 01. Miseria Cantare.lrc"), "{page}");

    let dir = format!("dir={}", data::encode(&artist.display().to_string()));
    let (status, _, _) = send(&s.web, form(&s.web, "/actions/fix-case", &dir)).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = std::fs::read_dir(&album)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"01. Miseria Cantare.lrc".to_string()),
        "{names:?}"
    );
    let (_, _, changes) = send(&s.web, get("/changes")).await;
    assert!(changes.contains("Fixed the file name"), "{changes}");
}

/// A multipart upload: `(name, file name or "", bytes)` parts.
fn multipart(web: &Web, parts: &[(&str, &str, &[u8])]) -> axum::http::Request<Body> {
    let boundary = "gatefoldboundary";
    let mut body = Vec::new();
    for (name, file, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        if file.is_empty() {
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
        } else {
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    axum::http::Request::post("/actions/upload")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-csrf-token", web.csrf.clone())
        .header("hx-request", "true")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn uploads_replace_and_keep_or_skip() {
    let s = setup("upload", "");
    let artist = s.library().join("AFI");
    let dir = artist.display().to_string();
    let png = b"\x89PNG\r\n\x1a\nnew logo";

    // Keep the upload: the old logo is archived, the new one kept.
    let request = multipart(
        &s.web,
        &[
            ("target", "", b"logo"),
            ("path", "", dir.as_bytes()),
            ("existing", "", b"keep_upload"),
            ("file", "logo.png", png),
        ],
    );
    let (status, headers, body) = send(&s.web, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        headers["hx-redirect"]
            .to_str()
            .unwrap()
            .contains("Uploaded%20logo.png")
    );
    assert_eq!(std::fs::read(artist.join("logo.png")).unwrap(), png);
    assert!(artist.join(".gatefold-logo-keep").is_file());
    let archived = std::fs::read_dir(artist.join(".gatefold-archive/logos"))
        .unwrap()
        .count();
    assert!(archived >= 1, "the old logo is archived");

    // Skip the current one: its contents are never written again.
    let newer = b"\x89PNG\r\n\x1a\nnewer logo";
    let request = multipart(
        &s.web,
        &[
            ("target", "", b"logo"),
            ("path", "", dir.as_bytes()),
            ("existing", "", b"skip_current"),
            ("file", "logo.png", newer),
        ],
    );
    assert_eq!(send(&s.web, request).await.0, StatusCode::OK);
    assert!(crate::skip::rejects(&artist, png));
    assert_eq!(std::fs::read(artist.join("logo.png")).unwrap(), newer);

    // Logos are PNGs.
    let request = multipart(
        &s.web,
        &[
            ("target", "", b"logo"),
            ("path", "", dir.as_bytes()),
            ("file", "logo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0]),
        ],
    );
    let (status, _, body) = send(&s.web, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("PNG"), "{body}");

    let (_, _, changes) = send(&s.web, get("/changes")).await;
    assert!(changes.contains("Uploaded"));
}

#[tokio::test]
async fn synced_lyrics_become_an_lrc_and_are_embedded() {
    let s = setup("lyrics", "");
    let album = s.library().join("AFI/Sing the Sorrow");
    let track = album.join("02. Silver and Cold.flac");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/joint-credit.flac"),
        &track,
    )
    .unwrap();
    let path = track.display().to_string();
    let request = multipart(
        &s.web,
        &[
            ("target", "", b"lyrics"),
            ("path", "", path.as_bytes()),
            (
                "text",
                "",
                b"[00:01.00]Your voice\r\n[00:04.00]so silver and cold",
            ),
        ],
    );
    let (status, _, body) = send(&s.web, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lrc = std::fs::read_to_string(album.join("02. Silver and Cold.lrc")).unwrap();
    assert_eq!(lrc, "[00:01.00]Your voice\n[00:04.00]so silver and cold\n");
    let tagged = lofty::read_from_path(&track).unwrap();
    let tag = lofty::file::TaggedFileExt::primary_tag(&tagged).unwrap();
    assert!(crate::writer::tag_has_lyrics(tag));
}

#[tokio::test]
async fn running_on_a_folder_stays_on_its_page() {
    let s = setup("run-folder", "");
    let artist = s.library().join("AFI");
    let dir = format!("dir={}", data::encode(&artist.display().to_string()));
    let (status, headers, _) = send(&s.web, form(&s.web, "/actions/run", &dir)).await;
    assert_eq!(status, StatusCode::OK);
    let to = headers["hx-redirect"].to_str().unwrap();
    assert!(to.starts_with("/artist?dir="), "{to}");
    assert!(to.contains("message=Starting%20a%20run."), "{to}");
    assert_eq!(s.web.daemon.queued(), 1);
}

#[tokio::test]
async fn archived_covers_are_listed_and_restored() {
    let s = setup("album-archive", "");
    let album = s.library().join("AFI/Decemberunderground");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/joint-credit.flac"),
        album.join("01. Prelude 12-21.flac"),
    )
    .unwrap();
    let dir = album.display().to_string();
    let upload = |bytes: &'static [u8]| {
        multipart(
            &s.web,
            &[
                ("target", "", b"cover"),
                ("path", "", dir.as_bytes()),
                ("existing", "", b"replace"),
                ("file", "cover.jpg", bytes),
            ],
        )
    };
    let first: &[u8] = b"\xff\xd8\xff\xe0first";
    let second: &[u8] = b"\xff\xd8\xff\xe0second";
    assert_eq!(send(&s.web, upload(first)).await.0, StatusCode::OK);
    assert_eq!(send(&s.web, upload(second)).await.0, StatusCode::OK);

    let (_, _, page) = send(&s.web, get(&data::album_link(&album))).await;
    assert!(page.contains("Archive · 1"), "{page}");
    let archived = std::fs::read_dir(album.join(".gatefold-archive/covers"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "jpg"))
        .unwrap();

    let path = format!(
        "path={}&keep=1",
        data::encode(&archived.display().to_string())
    );
    let (status, headers, _) = send(&s.web, form(&s.web, "/actions/restore-album", &path)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["hx-redirect"]
            .to_str()
            .unwrap()
            .contains("Restored%20cover.jpg")
    );
    assert_eq!(std::fs::read(album.join("cover.jpg")).unwrap(), first);
    assert!(album.join(".gatefold-cover-keep").is_file());
    assert!(
        crate::skip::rejects(&album, second),
        "the replaced cover is skipped"
    );
    let (_, _, changes) = send(&s.web, get("/changes")).await;
    assert!(changes.contains("Restored"));
}

#[tokio::test]
async fn jobs_show_on_the_dashboard_and_run_on_request() {
    let job = |name: &str, job: crate::config::Job, every: Option<u64>| crate::daemon::JobLane {
        name: name.to_string(),
        job,
        interval: every.map(std::time::Duration::from_secs),
    };
    let s = setup_with_jobs(
        "jobs",
        "",
        vec![
            job(
                "new-tracks",
                crate::config::Job {
                    new_only: true,
                    write_nfo: Some(true),
                    ..crate::config::Job::default()
                },
                Some(300),
            ),
            job(
                "artist-images",
                crate::config::Job {
                    only: Some(crate::config::Only::ArtistImages),
                    ..crate::config::Job::default()
                },
                None,
            ),
        ],
    );
    let (_, _, body) = send(&s.web, get("/")).await;
    assert!(
        body.contains("new-tracks") && body.contains("new items only · nfo"),
        "{body}"
    );
    assert!(body.contains("artist images only") && body.contains("only when asked"));
    assert!(body.contains("every 5m"));

    let run = |name: &str| form(&s.web, &format!("/actions/jobs/{name}/run"), "");
    let (status, _, body) = send(&s.web, run("artist-images")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Starting artist-images."), "{body}");
    assert_eq!(send(&s.web, run("nope")).await.0, StatusCode::NOT_FOUND);
}
