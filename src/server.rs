//! HTTP server: web UI, REST API (`/api/*`) and MCP over Streamable HTTP (`/mcp`).

use crate::app::{AResult, App, AppError};
use crate::mcp::{self, Session};
use crate::scope::Scope;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const COOKIE: &str = "mindcrab_session";

pub struct Config {
    pub web_token: String,
    pub mcp_token: String,
    pub mcp_read_only: bool,
    /// Default for the main MCP token: `remember` waits for approval.
    pub mcp_review: bool,
    pub cookie_secure: bool,
}

pub struct Shared {
    pub app: Arc<App>,
    pub cfg: Config,
    sessions: Mutex<HashMap<String, String>>,
    /// Failed web logins / API credentials, and failed MCP tokens, per client.
    login_limit: Limiter,
    mcp_limit: Limiter,
    /// Last ETag of each index-backed list, with the index generation it was computed at.
    list_tags: Mutex<HashMap<&'static str, (Gen, HeaderValue)>>,
}

type St = State<Arc<Shared>>;

fn eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

// ------------------------------------------------------------------ errors

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound(m) => ApiError(StatusCode::NOT_FOUND, m),
            AppError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
            AppError::BadRequest(m) => ApiError(StatusCode::BAD_REQUEST, m),
            AppError::Other(e) => {
                tracing::error!("{e:#}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Runs blocking work (SQLite, filesystem) off the async runtime.
async fn run<T, F>(st: &Arc<Shared>, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&App) -> AResult<T> + Send + 'static,
{
    let app = st.app.clone();
    tokio::task::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

// ------------------------------------------------------------------ router

pub fn router(shared: Arc<Shared>) -> Router {
    let api = Router::new()
        .route("/session", get(session_info))
        .route("/append", post(append))
        .route("/tree", get(tree))
        .route(
            "/note",
            get(get_note)
                .put(put_note)
                .delete(delete_note)
                .post(create_note),
        )
        .route("/note/rename", post(rename_note))
        .route("/folder/rename", post(rename_folder))
        .route("/related", get(related))
        .route("/embed-status", get(embed_status))
        .route("/templates", get(templates))
        .route("/template/render", post(render_template))
        .route("/folder", post(create_folder).delete(delete_folder))
        .route("/search", get(search))
        .route("/links", get(links))
        .route("/tags", get(tags))
        .route("/graph", get(graph))
        .route("/history", get(history))
        .route("/revision", get(revision))
        .route("/restore", post(restore))
        .route("/render", post(render))
        .route("/daily", post(daily))
        .route("/resolve", get(resolve))
        .route("/upload", post(upload))
        .route("/file/{*path}", get(file))
        .route("/mcp-info", get(mcp_info))
        .route("/ai-ignore", get(get_ai_ignore).put(put_ai_ignore))
        .route(
            "/connections",
            get(list_connections).post(create_connection),
        )
        .route(
            "/connections/{id}",
            axum::routing::put(update_connection).delete(delete_connection),
        )
        .route("/activity", get(activity))
        .route("/activity/{id}", get(activity_diff))
        .route("/activity/{id}/undo", post(activity_undo))
        .route("/pending", get(pending))
        .route("/pending/{id}/approve", post(pending_approve))
        .route("/pending/{id}/reject", post(pending_reject))
        .route("/tasks", get(tasks).post(add_task))
        .route("/tasks/toggle", post(toggle_task))
        .route("/base", post(base_query))
        .route("/memory-review", get(memory_review))
        .route("/remove-line", post(remove_line))
        .route("/preview", get(preview))
        .route("/canvas", get(get_canvas).put(put_canvas))
        .layer(middleware::from_fn_with_state(shared.clone(), etag))
        .layer(middleware::from_fn_with_state(shared.clone(), require_auth))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/health", get(|| async { "ok" }));

    Router::new()
        .nest("/api", api)
        // Trailing-slash variants too: a connector URL ending in `/` would otherwise
        // hit the web UI and get 405 for POST.
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .route("/mcp/", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .route(
            "/mcp/{token}",
            post(mcp_post_path).get(mcp_get).delete(mcp_delete_path),
        )
        .route(
            "/mcp/{token}/",
            post(mcp_post_path).get(mcp_get).delete(mcp_delete_path),
        )
        .merge(crate::web::routes())
        .layer(DefaultBodyLimit::max(
            crate::vault::MAX_FILE_BYTES + 1024 * 1024,
        ))
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true)
                .br(true),
        )
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(|req: &Request| {
                tracing::debug_span!(
                    "request",
                    method = %req.method(),
                    uri = %log_uri(req.uri()),
                    version = ?req.version(),
                )
            }),
        )
        .with_state(shared)
}

/// Request URI for logs, with the secret part of `/mcp/{token}` URLs masked.
fn log_uri(uri: &Uri) -> String {
    match uri.path().strip_prefix("/mcp/") {
        Some(rest) if !rest.is_empty() => "/mcp/<token>".into(),
        _ => uri.to_string(),
    }
}

/// Largest JSON body the ETag layer buffers; bigger responses pass through untouched.
const ETAG_MAX: u64 = 32 << 20;

/// Index generation: rows written through the index connection (`total_changes`),
/// commits made by any other connection (`data_version`), and the `.mcpignore` mtime.
type Gen = (u64, i64, Option<std::time::SystemTime>);

/// GET endpoints whose JSON depends only on the index (and `.mcpignore`), so an unchanged
/// [`Gen`] means an unchanged body.
fn gen_route(path: &str) -> Option<&'static str> {
    match path.strip_prefix("/api").unwrap_or(path) {
        "/tree" => Some("tree"),
        "/tags" => Some("tags"),
        "/graph" => Some("graph"),
        _ => None,
    }
}

fn index_gen(app: &App) -> AResult<Gen> {
    let idx = app.idx();
    let version: i64 = idx
        .conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .map_err(anyhow::Error::from)?;
    let changes = idx.conn.total_changes();
    drop(idx);
    let ignore = std::fs::metadata(app.vault.root().join(crate::features::IGNORE_FILE))
        .and_then(|m| m.modified())
        .ok();
    Ok((changes, version, ignore))
}

fn etag_matches(inm: Option<&HeaderValue>, tag: &HeaderValue) -> bool {
    inm.is_some_and(|v| {
        v.as_bytes()
            .split(|b| *b == b',')
            .any(|t| t.trim_ascii() == tag.as_bytes())
    })
}

fn not_modified(tag: HeaderValue) -> Response {
    let mut r = StatusCode::NOT_MODIFIED.into_response();
    r.headers_mut().insert(header::ETAG, tag);
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    r
}

/// Weak ETag over the JSON body for GET requests, answering 304 when the client
/// already has it — the UI polls the tree and tags, which rarely change. For the
/// index-backed lists, a 304 is answered from the index generation alone, without
/// running the query.
async fn etag(State(st): St, req: Request, next: Next) -> Response {
    if req.method() != axum::http::Method::GET {
        return next.run(req).await;
    }
    let inm = req.headers().get(header::IF_NONE_MATCH).cloned();
    let route = gen_route(req.uri().path());
    // Read before the handler runs: a write racing the query then only costs a miss.
    let generation = match route {
        Some(_) => run(&st, index_gen).await.ok(),
        None => None,
    };
    if let (Some(r), Some(g)) = (route, generation.as_ref()) {
        let known = st
            .list_tags
            .lock()
            .unwrap()
            .get(r)
            .filter(|(at, _)| at == g)
            .map(|(_, tag)| tag.clone());
        if let Some(tag) = known
            && etag_matches(inm.as_ref(), &tag)
        {
            return not_modified(tag);
        }
    }
    let res = next.run(req).await;
    let json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    let small = axum::body::HttpBody::size_hint(res.body())
        .upper()
        .is_some_and(|n| n <= ETAG_MAX);
    if res.status() != StatusCode::OK || !json || !small || res.headers().contains_key(header::ETAG)
    {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, ETAG_MAX as usize).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let tag = {
        use sha2::{Digest, Sha256};
        format!("W/\"{}\"", hex::encode(&Sha256::digest(&bytes)[..12]))
    };
    let tag = HeaderValue::from_str(&tag).expect("hex etag");
    if let (Some(r), Some(g)) = (route, generation) {
        st.list_tags.lock().unwrap().insert(r, (g, tag.clone()));
    }
    if etag_matches(inm.as_ref(), &tag) {
        return not_modified(tag);
    }
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    parts.headers.insert(header::ETAG, tag);
    Response::from_parts(parts, Body::from(bytes))
}

pub fn shared(app: Arc<App>, cfg: Config) -> Arc<Shared> {
    Arc::new(Shared {
        app,
        cfg,
        sessions: Mutex::new(HashMap::new()),
        login_limit: Limiter::new(),
        mcp_limit: Limiter::new(),
        list_tags: Mutex::new(HashMap::new()),
    })
}

async fn require_auth(State(st): St, jar: CookieJar, req: Request, next: Next) -> Response {
    let login_required =
        || ApiError(StatusCode::UNAUTHORIZED, "login required".into()).into_response();
    let presented: Vec<String> = bearer(req.headers())
        .into_iter()
        .chain(jar.get(COOKIE).map(|c| c.value()))
        .map(str::to_string)
        .collect();
    if presented.is_empty() {
        return login_required();
    }
    // A bearer header or cookie is a guess at the web token like a login is, so it is
    // throttled the same way — and refused unchecked while the client is locked out.
    let who = client_key(req.extensions(), req.headers());
    if let Err(wait) = st.login_limit.check(&who) {
        return too_many(wait);
    }
    if !presented.iter().any(|t| eq(t, &st.cfg.web_token)) {
        for t in &presented {
            st.login_limit.fail(&who, t);
        }
        return login_required();
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct LoginReq {
    token: String,
}

async fn login(
    State(st): St,
    ext: Extensions,
    headers: HeaderMap,
    jar: CookieJar,
    Json(r): Json<LoginReq>,
) -> Response {
    let who = client_key(&ext, &headers);
    if let Err(wait) = st.login_limit.check(&who) {
        return too_many(wait);
    }
    let token = r.token.trim();
    if !eq(token, &st.cfg.web_token) {
        st.login_limit.fail(&who, token);
        tokio::time::sleep(Duration::from_millis(800)).await;
        return ApiError(StatusCode::UNAUTHORIZED, "wrong token".into()).into_response();
    }
    st.login_limit.clear(&who);
    let cookie = Cookie::build((COOKIE, st.cfg.web_token.clone()))
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(st.cfg.cookie_secure)
        .path("/")
        .permanent()
        .build();
    (jar.add(cookie), Json(json!({"ok": true}))).into_response()
}

async fn logout(jar: CookieJar) -> Response {
    (
        jar.remove(Cookie::build(COOKIE).path("/")),
        Json(json!({"ok": true})),
    )
        .into_response()
}

// ------------------------------------------------------------------ notes

#[derive(Deserialize)]
struct PathQ {
    path: String,
}

async fn tree(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        let idx = app.idx();
        let notes = idx.list(None, false, 1_000_000)?;
        let folders = idx.folders()?;
        let pending = idx.pending_count()?;
        drop(idx);
        Ok(Json(json!({
            "notes": notes,
            "folders": folders,
            "canvases": app.canvases()?,
            "hidden": app.ai_hidden_paths()?,
            "pending": pending,
        })))
    })
    .await
}

async fn get_note(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(serde_json::to_value(app.read_note(&q.path)?).unwrap()))
    })
    .await
}

#[derive(Deserialize)]
struct PutNote {
    path: String,
    content: String,
    expected_hash: Option<String>,
}

async fn put_note(State(st): St, Json(r): Json<PutNote>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let w = app.write_note(&r.path, &r.content, r.expected_hash.as_deref(), "web")?;
        Ok(Json(serde_json::to_value(w).unwrap()))
    })
    .await
}

#[derive(Deserialize)]
struct CreateNote {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

async fn create_note(State(st): St, Json(r): Json<CreateNote>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let path = crate::vault::Vault::clean_note(&r.path)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let title = crate::note::stem(&path).to_string();
        let content = match (r.content, r.template.as_deref().filter(|t| !t.is_empty())) {
            (Some(c), _) => c,
            (None, Some(t)) => app.render_template_note(t, &title)?,
            (None, None) => format!("# {title}\n\n"),
        };
        let w = app.create_note(&path, &content, "web")?;
        Ok(Json(serde_json::to_value(w).unwrap()))
    })
    .await
}

async fn delete_note(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.delete_note(&q.path, "web")?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

#[derive(Deserialize)]
struct RenameReq {
    from: String,
    to: String,
}

async fn rename_note(State(st): St, Json(r): Json<RenameReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let (to, updated) = app.rename_note(&r.from, &r.to, "web")?;
        Ok(Json(json!({"path": to, "updated_links": updated})))
    })
    .await
}

async fn create_folder(State(st): St, Json(r): Json<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(json!({"path": app.create_folder(&r.path)?})))
    })
    .await
}

async fn delete_folder(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.delete_folder(&q.path)?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    limit: Option<usize>,
    mode: Option<String>,
}

async fn search(State(st): St, Query(q): Query<SearchQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let out = app.search(
            &q.q,
            q.limit.unwrap_or(30).min(200),
            q.mode.as_deref().unwrap_or("auto"),
        )?;
        Ok(Json(serde_json::to_value(out).unwrap()))
    })
    .await
}

async fn related(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(json!({"related": app.related(&q.path, 10)?})))
    })
    .await
}

async fn embed_status(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| Ok(Json(app.embed_status()))).await
}

async fn templates(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        Ok(Json(
            json!({"templates": app.templates()?, "folder": app.settings.templates_dir}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct TemplateReq {
    template: String,
    title: String,
}

async fn render_template(State(st): St, Json(r): Json<TemplateReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            json!({"content": app.render_template_note(&r.template, &r.title)?}),
        ))
    })
    .await
}

async fn rename_folder(State(st): St, Json(r): Json<RenameReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let (to, moved, updated) = app.rename_folder(&r.from, &r.to, "web")?;
        Ok(Json(
            json!({"path": to, "moved": moved, "updated_links": updated}),
        ))
    })
    .await
}

async fn links(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let idx = app.idx();
        Ok(Json(json!({
            "backlinks": idx.backlinks(&q.path)?,
            "outgoing": idx.outgoing(&q.path)?,
            "unlinked": idx.unlinked_mentions(&q.path, 20)?,
        })))
    })
    .await
}

async fn tags(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        let t: Vec<Value> = app
            .idx()
            .tag_counts()?
            .into_iter()
            .map(|(t, c)| json!({"tag": t, "count": c}))
            .collect();
        Ok(Json(json!({"tags": t})))
    })
    .await
}

async fn graph(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        Ok(Json(serde_json::to_value(app.idx().graph()?).unwrap()))
    })
    .await
}

async fn history(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(json!({"revisions": app.idx().revisions(&q.path)?})))
    })
    .await
}

#[derive(Deserialize)]
struct IdQ {
    id: i64,
}

async fn revision(State(st): St, Query(q): Query<IdQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let (path, content) = app
            .idx()
            .revision(q.id)?
            .ok_or_else(|| AppError::NotFound("revision not found".into()))?;
        Ok(Json(json!({"id": q.id, "path": path, "content": content})))
    })
    .await
}

async fn restore(State(st): St, Json(r): Json<IdQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.restore_revision(r.id, "web")?).unwrap(),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct RenderReq {
    path: String,
    content: String,
}

async fn render(State(st): St, Json(r): Json<RenderReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let (fm, _) = crate::note::split_frontmatter(&r.content);
        let fm = fm.map(crate::note::parse_frontmatter).unwrap_or_default();
        Ok(Json(
            json!({"html": app.render(&r.path, &r.content)?, "frontmatter": fm}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct DailyReq {
    date: Option<String>,
    period: Option<String>,
}

async fn daily(State(st): St, Json(r): Json<DailyReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            json!({"path": app.ensure_period(r.period.as_deref().unwrap_or("day"), r.date.as_deref(), "web")?}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct ResolveQ {
    from: String,
    target: String,
}

async fn resolve(State(st): St, Query(q): Query<ResolveQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            json!({"path": app.idx().resolve_link(&q.from, &q.target)?}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct UploadQ {
    name: String,
}

async fn upload(State(st): St, Query(q): Query<UploadQ>, body: Bytes) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(json!({"path": app.upload(&q.name, &body)?})))
    })
    .await
}

/// Files up to this size are read in one go; bigger ones are streamed from disk.
const STREAM_ABOVE: u64 = 256 << 10;

/// Absolute path and size of a vault file, refusing anything a symlink would take
/// outside the vault.
fn vault_file(app: &App, path: &str) -> AResult<(std::path::PathBuf, u64)> {
    let clean =
        crate::vault::Vault::clean(path).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let missing = || AppError::NotFound(format!("file not found: {clean}"));
    let real = app
        .vault
        .abs(&clean)
        .canonicalize()
        .map_err(|_| missing())?;
    let meta = std::fs::metadata(&real).map_err(|_| missing())?;
    if !real.starts_with(app.vault.root()) || !meta.is_file() {
        return Err(missing());
    }
    Ok((real, meta.len()))
}

#[derive(Debug, PartialEq)]
enum ByteRange {
    Whole,
    /// Inclusive first and last byte.
    Part(u64, u64),
    Unsatisfiable,
}

/// Parses a `Range` header against a file of `size` bytes. Only a single `bytes=` range is
/// honoured (what media players and PDF viewers send); anything else gets the whole
/// file, which RFC 9110 allows.
fn byte_range(spec: Option<&str>, size: u64) -> ByteRange {
    let Some(spec) = spec.map(str::trim) else {
        return ByteRange::Whole;
    };
    let Some(set) = spec
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &spec[6..])
    else {
        return ByteRange::Whole;
    };
    let Some((a, b)) = set.split_once('-').filter(|_| !set.contains(',')) else {
        return ByteRange::Whole;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix range: the last `n` bytes.
        return match b.parse::<u64>() {
            Ok(0) => ByteRange::Unsatisfiable,
            Ok(_) if size == 0 => ByteRange::Unsatisfiable,
            Ok(n) => ByteRange::Part(size.saturating_sub(n), size - 1),
            Err(_) => ByteRange::Whole,
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return ByteRange::Whole;
    };
    let end = match b {
        "" => u64::MAX,
        b => match b.parse::<u64>() {
            Ok(e) if e >= start => e,
            _ => return ByteRange::Whole,
        },
    };
    if start >= size {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Part(start, end.min(size - 1))
}

enum FileBody {
    Memory(Bytes),
    Disk(tokio::fs::File),
}

async fn file(State(st): St, Path(path): Path<String>, headers: HeaderMap) -> ApiResult<Response> {
    let p = path.clone();
    let (abs, size) = run(&st, move |app| vault_file(app, &p)).await?;
    let io_err = |e: std::io::Error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let (source, size) = if size <= STREAM_ABOVE {
        let p = path.clone();
        let bytes = Bytes::from(run(&st, move |app| app.read_file(&p)).await?);
        let n = bytes.len() as u64;
        (FileBody::Memory(bytes), n)
    } else {
        let f = tokio::fs::File::open(&abs).await.map_err(io_err)?;
        (FileBody::Disk(f), size)
    };
    // A conditional range (`If-Range`) cannot match: we send no validators.
    let range = headers
        .get(header::RANGE)
        .filter(|_| !headers.contains_key(header::IF_RANGE))
        .and_then(|v| v.to_str().ok());
    let (start, len, partial) = match byte_range(range, size) {
        ByteRange::Whole => (0, size, false),
        ByteRange::Part(a, b) => (a, b - a + 1, true),
        ByteRange::Unsatisfiable => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).expect("ascii"),
            );
            return Ok(r);
        }
    };
    let body = match source {
        FileBody::Memory(b) => Body::from(b.slice(start as usize..(start + len) as usize)),
        FileBody::Disk(mut f) => {
            if start > 0 {
                f.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(io_err)?;
            }
            Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                f.take(len),
                64 << 10,
            ))
        }
    };
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if partial {
        // A 206 also keeps the compression layer away from media streams.
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
                .expect("ascii"),
        );
    }
    // SVG and HTML could run scripts when opened directly; sandbox them.
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let safe_inline = mime.type_() == "image"
        || mime.essence_str() == "application/pdf"
        || mime.type_() == "audio"
        || mime.type_() == "video";
    let ct = if mime.essence_str() == "text/html" {
        "text/plain; charset=utf-8".to_string()
    } else {
        mime.to_string()
    };
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if !safe_inline {
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    if partial {
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    Ok(resp)
}

async fn mcp_info(State(st): St, headers: HeaderMap) -> Json<Value> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3436");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    let base = format!("{proto}://{host}");
    Json(json!({
        "url": format!("{base}/mcp"),
        "url_with_token": format!("{base}/mcp/{}", st.cfg.mcp_token),
        "token": st.cfg.mcp_token,
        "read_only": st.cfg.mcp_read_only,
        "separate_token": st.cfg.mcp_token != st.cfg.web_token,
    }))
}

// ------------------------------------------------------------------ MCP

/// Main MCP token → full scope; otherwise a named connection's token.
async fn resolve_scope(st: &Arc<Shared>, token: &str) -> Option<(Scope, bool)> {
    if eq(token, &st.cfg.mcp_token) {
        return Some((
            Scope::full("default", st.cfg.mcp_read_only, st.cfg.mcp_review),
            false,
        ));
    }
    if !token.starts_with("mcc_") {
        return None;
    }
    let app = st.app.clone();
    let t = token.to_string();
    tokio::task::spawn_blocking(move || app.idx().connection_scope(&t).ok().flatten())
        .await
        .ok()
        .flatten()
        .map(|s| (s, true))
}

fn mcp_unauthorized() -> Response {
    let mut r = (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "invalid or missing MCP token"})),
    )
        .into_response();
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"mindcrab\""),
    );
    r
}

/// Resolves the token of an MCP request (bearer header or `/mcp/{token}` path). A wrong
/// token is answered 401 (a 404 made MCP clients report "not implemented"); failures
/// are throttled per client.
// The error is a ready-made axum Response, returned straight to the client.
#[allow(clippy::result_large_err)]
async fn mcp_auth(
    st: &Arc<Shared>,
    ext: &Extensions,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<(Scope, bool), Response> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Err(mcp_unauthorized());
    };
    let who = client_key(ext, headers);
    st.mcp_limit.check(&who).map_err(too_many)?;
    if let Some(scope) = resolve_scope(st, token).await {
        return Ok(scope);
    }
    st.mcp_limit.fail(&who, token);
    tokio::time::sleep(Duration::from_millis(500)).await;
    Err(mcp_unauthorized())
}

async fn mcp_post(State(st): St, ext: Extensions, headers: HeaderMap, body: Bytes) -> Response {
    let auth = mcp_auth(&st, &ext, &headers, bearer(&headers)).await;
    match auth {
        Ok(scope) => mcp_dispatch(st, headers, body, scope).await,
        Err(r) => r,
    }
}

async fn mcp_post_path(
    State(st): St,
    Path(token): Path<String>,
    ext: Extensions,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = mcp_auth(&st, &ext, &headers, Some(&token)).await;
    match auth {
        Ok(scope) => mcp_dispatch(st, headers, body, scope).await,
        Err(r) => r,
    }
}

/// The `MCP-Protocol-Version` (header or 2026-style `_meta`) of a non-`initialize`
/// request, when this server does not speak it.
fn unsupported_version(headers: &HeaderMap, msg: &Value) -> Option<String> {
    if msg.get("method").and_then(Value::as_str) == Some("initialize") {
        return None;
    }
    let header = headers
        .get("mcp-protocol-version")
        .map(|v| v.to_str().unwrap_or("?").trim().to_string());
    let meta = msg
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    [header, meta]
        .into_iter()
        .flatten()
        .find(|v| !mcp::SUPPORTED_VERSIONS.contains(&v.as_str()))
}

async fn mcp_dispatch(
    st: Arc<Shared>,
    headers: HeaderMap,
    body: Bytes,
    (scope, named): (Scope, bool),
) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            )
                .into_response();
        }
    };
    // Spec (2025-06-18+): unsupported protocol version → 400. The plain JSON-RPC error
    // (not a 2026 "modern" one) tells dual-era clients to fall back to `initialize`.
    if let Some(v) = unsupported_version(&headers, &msg) {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let message = format!(
            "unsupported MCP-Protocol-Version {v:?}; supported: {} (start with initialize)",
            mcp::SUPPORTED_VERSIONS.join(", ")
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": message}}),
            ),
        )
            .into_response();
    }
    let is_init = msg.get("method").and_then(Value::as_str) == Some("initialize");
    let sid_in = headers
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);
    let fallback = headers
        .get(header::USER_AGENT)
        .and_then(|h| h.to_str().ok())
        .and_then(|ua| ua.split(['/', ' ']).next())
        .filter(|s| !s.is_empty())
        .unwrap_or("mcp")
        .to_lowercase();
    let client = sid_in
        .as_ref()
        .and_then(|sid| st.sessions.lock().unwrap().get(sid).cloned())
        .unwrap_or(fallback);
    let app = st.app.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::new(scope, named);
        session.client = client;
        let out = mcp::handle(&app, &mut session, msg);
        (out, session.client)
    })
    .await;
    let (out, client) = match result {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut new_sid = None;
    if is_init {
        let mut b = [0u8; 16];
        let _ = getrandom::fill(&mut b);
        let sid = hex::encode(b);
        let mut map = st.sessions.lock().unwrap();
        if map.len() > 2000 {
            map.clear();
        }
        map.insert(sid.clone(), client);
        new_sid = Some(sid);
    }

    let mut resp = match out {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    };
    if let Some(sid) = new_sid
        && let Ok(v) = HeaderValue::from_str(&sid)
    {
        resp.headers_mut().insert("mcp-session-id", v);
    }
    resp
}

async fn mcp_get() -> Response {
    // No server-initiated stream; clients fall back to plain request/response.
    let mut r = StatusCode::METHOD_NOT_ALLOWED.into_response();
    r.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    r
}

fn end_session(st: &Shared, headers: &HeaderMap) -> Response {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|h| h.to_str().ok()) {
        st.sessions.lock().unwrap().remove(sid);
    }
    StatusCode::OK.into_response()
}

/// Session termination needs the same credentials as the requests of the session.
async fn mcp_delete(State(st): St, ext: Extensions, headers: HeaderMap) -> Response {
    let auth = mcp_auth(&st, &ext, &headers, bearer(&headers)).await;
    match auth {
        Ok(_) => end_session(&st, &headers),
        Err(r) => r,
    }
}

async fn mcp_delete_path(
    State(st): St,
    Path(token): Path<String>,
    ext: Extensions,
    headers: HeaderMap,
) -> Response {
    let auth = mcp_auth(&st, &ext, &headers, Some(&token)).await;
    match auth {
        Ok(_) => end_session(&st, &headers),
        Err(r) => r,
    }
}

// ------------------------------------------------------------------ auth throttling

/// Rate-limit key for a client: its IP address (IPv6 by /64). When the peer is a
/// reverse proxy on a loopback/private address, the last `X-Forwarded-For` hop — the
/// address the proxy itself saw — names the client instead.
fn client_key(ext: &Extensions, headers: &HeaderMap) -> String {
    let Some(ConnectInfo(peer)) = ext.get::<ConnectInfo<SocketAddr>>() else {
        return "-".into();
    };
    let mut ip = canonical_ip(peer.ip());
    if is_local(ip)
        && let Some(fwd) = headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        ip = canonical_ip(fwd);
    }
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        // Loopback, unique-local (fc00::/7) and link-local (fe80::/10).
        IpAddr::V6(v) => {
            v.is_loopback()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn too_many(wait: Duration) -> Response {
    let mut r = ApiError(
        StatusCode::TOO_MANY_REQUESTS,
        "too many failed attempts, try again later".into(),
    )
    .into_response();
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(wait.as_secs() + 1));
    r
}

/// Free failed attempts before a client is locked out.
const FREE_FAILS: u32 = 5;
const MAX_LOCK: Duration = Duration::from_secs(300);
/// A client's failures are forgotten after this long without a new one.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);
const MAX_CLIENTS: usize = 4096;
/// Shared bucket for new clients while the table is full of active ones.
const OVERFLOW: &str = "*";

/// Failed-authentication throttle per client: a few free attempts, then an exponential
/// lockout (1 s, 2 s, 4 s … up to 5 min). While locked out, requests are refused before
/// the credential is compared, so firing many requests in parallel buys no extra
/// guesses. Repeating the same wrong credential (a stale cookie, a misconfigured MCP
/// client) counts once.
struct Limiter {
    clients: Mutex<HashMap<String, Strikes>>,
    hasher: std::collections::hash_map::RandomState,
}

struct Strikes {
    fails: u32,
    last: Instant,
    until: Option<Instant>,
    /// Hashes of the latest distinct wrong credentials.
    recent: [u64; 4],
}

impl Limiter {
    fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            hasher: Default::default(),
        }
    }

    fn slot<'k>(map: &HashMap<String, Strikes>, key: &'k str) -> &'k str {
        if map.contains_key(key) || map.len() < MAX_CLIENTS {
            key
        } else {
            OVERFLOW
        }
    }

    /// `Err(retry_after)` while `key` is locked out.
    fn check(&self, key: &str) -> Result<(), Duration> {
        let map = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match map.get(Self::slot(&map, key)).and_then(|s| s.until) {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    fn fail(&self, key: &str, credential: &str) {
        let hash = self.hasher.hash_one(credential);
        let mut map = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let active = |s: &Strikes| {
            s.until.is_some_and(|u| u > now) || now.duration_since(s.last) < FORGET_AFTER
        };
        if !map.contains_key(key) && map.len() >= MAX_CLIENTS {
            map.retain(|_, s| active(s));
        }
        let slot = Self::slot(&map, key).to_string();
        let s = map.entry(slot).or_insert(Strikes {
            fails: 0,
            last: now,
            until: None,
            recent: [0; 4],
        });
        if !active(s) {
            s.fails = 0;
            s.recent = [0; 4];
        }
        s.last = now;
        if s.recent.contains(&hash) {
            return;
        }
        s.recent.rotate_right(1);
        s.recent[0] = hash;
        s.fails = s.fails.saturating_add(1);
        if s.fails >= FREE_FAILS {
            let lock = Duration::from_secs(1 << (s.fails - FREE_FAILS).min(16));
            s.until = Some(now + lock.min(MAX_LOCK));
        }
    }

    fn clear(&self, key: &str) {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

// ------------------------------------------------------------------ privacy & connections

async fn get_ai_ignore(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        Ok(Json(
            json!({"text": app.ai_ignore_text(), "hidden": app.ai_hidden_paths()?}),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct TextReq {
    text: String,
}

async fn put_ai_ignore(State(st): St, Json(r): Json<TextReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.set_ai_ignore(&r.text)?;
        Ok(Json(json!({"hidden": app.ai_hidden_paths()?})))
    })
    .await
}

#[derive(Deserialize)]
struct ConnReq {
    name: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    review: bool,
    #[serde(default)]
    folders: Vec<String>,
}

async fn list_connections(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| {
        Ok(Json(json!({"connections": app.connections()?})))
    })
    .await
}

async fn create_connection(State(st): St, Json(r): Json<ConnReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let (id, token) = app.create_connection(&r.name, r.read_only, r.review, &r.folders)?;
        Ok(Json(json!({"id": id, "token": token})))
    })
    .await
}

async fn update_connection(
    State(st): St,
    Path(id): Path<i64>,
    Json(r): Json<ConnReq>,
) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.update_connection(id, &r.name, r.read_only, r.review, &r.folders)?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

async fn delete_connection(State(st): St, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.delete_connection(id)?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

// ------------------------------------------------------------------ activity & review

#[derive(Deserialize)]
struct ActivityQ {
    #[serde(default, deserialize_with = "flag")]
    all: bool,
    before: Option<i64>,
    limit: Option<usize>,
}

async fn activity(State(st): St, Query(q): Query<ActivityQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            json!({"items": app.activity(!q.all, q.before, q.limit.unwrap_or(50))?}),
        ))
    })
    .await
}

async fn activity_diff(State(st): St, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    run(&st, move |app| Ok(Json(app.activity_diff(id)?))).await
}

async fn activity_undo(State(st): St, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(json!({"path": app.undo_activity(id, "web")?})))
    })
    .await
}

async fn pending(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| Ok(Json(json!({"facts": app.pending_facts()?})))).await
}

#[derive(Deserialize, Default)]
struct ApproveReq {
    fact: Option<String>,
    topic: Option<String>,
}

async fn pending_approve(
    State(st): St,
    Path(id): Path<i64>,
    body: Option<Json<ApproveReq>>,
) -> ApiResult<Json<Value>> {
    let r = body.map(|b| b.0).unwrap_or_default();
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.approve_fact(id, r.fact.as_deref(), r.topic.as_deref())?)
                .unwrap(),
        ))
    })
    .await
}

async fn pending_reject(State(st): St, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        app.reject_fact(id)?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

async fn memory_review(State(st): St) -> ApiResult<Json<Value>> {
    run(&st, |app| Ok(Json(app.memory_review(None)?))).await
}

#[derive(Deserialize)]
struct LineReq {
    path: String,
    line: usize,
    raw: String,
}

async fn remove_line(State(st): St, Json(r): Json<LineReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.remove_line(&r.path, r.line, &r.raw, "web")?).unwrap(),
        ))
    })
    .await
}

// ------------------------------------------------------------------ tasks, bases, previews, canvases

#[derive(Deserialize)]
struct TasksQ {
    #[serde(default, deserialize_with = "flag")]
    done: bool,
    /// Only count open tasks due on or before this day (`YYYY-MM-DD`) — for the nav badge.
    due_count: Option<String>,
}

/// Accepts `1`, `true`, `yes`, `on` (query-string friendly booleans).
fn flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    Ok(matches!(
        s.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ))
}

async fn tasks(State(st): St, Query(q): Query<TasksQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        if let Some(day) = q.due_count {
            return Ok(Json(json!({"due": app.idx().count_due_tasks(&day)?})));
        }
        let f = crate::features::TaskFilter {
            include_done: q.done,
            ..Default::default()
        };
        Ok(Json(json!({"tasks": app.tasks(&f, None)?})))
    })
    .await
}

#[derive(Deserialize)]
struct AddTaskReq {
    text: String,
    due: Option<String>,
    path: Option<String>,
}

async fn add_task(State(st): St, Json(r): Json<AddTaskReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.add_task(
                &r.text,
                r.due.as_deref(),
                r.path.as_deref(),
                "web",
            )?)
            .unwrap(),
        ))
    })
    .await
}

async fn toggle_task(State(st): St, Json(r): Json<LineReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.toggle_task(&r.path, r.line, &r.raw, "web")?).unwrap(),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct BaseReq {
    spec: String,
}

async fn base_query(State(st): St, Json(r): Json<BaseReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let spec = crate::bases::parse_spec(&r.spec).map_err(AppError::BadRequest)?;
        Ok(Json(
            serde_json::to_value(app.base_query(&spec, None)?).unwrap(),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct PreviewQ {
    path: String,
    anchor: Option<String>,
}

async fn preview(State(st): St, Query(q): Query<PreviewQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let path = app.find_note(&q.path)?;
        Ok(Json(json!({"path": path, "title": app.title_of(&path), "html": app.preview(&path, q.anchor.as_deref())?})))
    })
    .await
}

async fn get_canvas(State(st): St, Query(q): Query<PathQ>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        let text = app.read_canvas(&q.path)?;
        Ok(Json(
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"nodes": [], "edges": []})),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct CanvasReq {
    path: String,
    content: Value,
}

async fn put_canvas(State(st): St, Json(r): Json<CanvasReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            json!({"path": app.write_canvas(&r.path, &r.content.to_string(), "web")?}),
        ))
    })
    .await
}

async fn session_info(State(st): St) -> Json<Value> {
    let s = &st.app.settings;
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "settings": {
            "daily_dir": s.daily_dir,
            "memory_dir": s.memory_dir,
            "templates_dir": s.templates_dir,
            "attachments_dir": s.attachments_dir,
        }
    }))
}

#[derive(Deserialize)]
struct AppendReq {
    path: String,
    text: String,
    heading: Option<String>,
}

async fn append(State(st): St, Json(r): Json<AppendReq>) -> ApiResult<Json<Value>> {
    run(&st, move |app| {
        Ok(Json(
            serde_json::to_value(app.append(&r.path, &r.text, r.heading.as_deref(), "web")?)
                .unwrap(),
        ))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_locks_out_after_free_attempts() {
        let l = Limiter::new();
        for i in 0..FREE_FAILS - 1 {
            assert!(l.check("1.2.3.4").is_ok());
            l.fail("1.2.3.4", &format!("guess{i}"));
        }
        // The same wrong credential again (stale cookie) does not count.
        l.fail("1.2.3.4", "guess0");
        assert!(l.check("1.2.3.4").is_ok());
        l.fail("1.2.3.4", "another");
        assert!(l.check("1.2.3.4").is_err());
        assert!(l.check("5.6.7.8").is_ok(), "other clients are not affected");
        l.clear("1.2.3.4");
        assert!(l.check("1.2.3.4").is_ok());
    }

    #[test]
    fn limiter_table_is_bounded() {
        let l = Limiter::new();
        for i in 0..MAX_CLIENTS + 50 {
            l.fail(&format!("c{i}"), "x");
        }
        assert!(l.clients.lock().unwrap().len() <= MAX_CLIENTS + 1);
        // New clients share the overflow bucket, which locks out like any other.
        for i in 0..FREE_FAILS {
            l.fail("new", &format!("g{i}"));
        }
        assert!(l.check("new").is_err() && l.check("newer").is_err());
    }

    #[test]
    fn client_keys() {
        let ext = |addr: &str| {
            let mut e = Extensions::new();
            e.insert(ConnectInfo(addr.parse::<SocketAddr>().unwrap()));
            e
        };
        let mut fwd = HeaderMap::new();
        fwd.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 9.8.7.6"),
        );
        assert_eq!(client_key(&ext("127.0.0.1:1"), &fwd), "9.8.7.6");
        assert_eq!(client_key(&ext("172.18.0.2:1"), &fwd), "9.8.7.6");
        // A public peer cannot pick its own key.
        assert_eq!(client_key(&ext("1.2.3.4:1"), &fwd), "1.2.3.4");
        let none = HeaderMap::new();
        assert_eq!(
            client_key(&ext("[2001:db8:1:2:3::4]:1"), &none),
            "2001:db8:1:2::/64"
        );
        assert_eq!(client_key(&ext("[::ffff:1.2.3.4]:1"), &none), "1.2.3.4");
        assert_eq!(client_key(&Extensions::new(), &none), "-");
    }

    #[test]
    fn byte_ranges() {
        use ByteRange::*;
        assert_eq!(byte_range(None, 10), Whole);
        assert_eq!(byte_range(Some("bytes=0-"), 10), Part(0, 9));
        assert_eq!(byte_range(Some("bytes=2-4"), 10), Part(2, 4));
        assert_eq!(byte_range(Some("bytes=5-100"), 10), Part(5, 9));
        assert_eq!(byte_range(Some("bytes=-3"), 10), Part(7, 9));
        assert_eq!(byte_range(Some("bytes=-30"), 10), Part(0, 9));
        assert_eq!(byte_range(Some("bytes=10-"), 10), Unsatisfiable);
        assert_eq!(byte_range(Some("bytes=-0"), 10), Unsatisfiable);
        assert_eq!(byte_range(Some("bytes=0-1,4-5"), 10), Whole);
        assert_eq!(byte_range(Some("bytes=4-2"), 10), Whole);
        assert_eq!(byte_range(Some("items=0-1"), 10), Whole);
        assert_eq!(byte_range(Some("bytes=x-"), 10), Whole);
    }

    #[test]
    fn mcp_token_is_not_logged() {
        let uri: Uri = "/mcp/mc_secret?x=1".parse().unwrap();
        assert_eq!(log_uri(&uri), "/mcp/<token>");
        let uri: Uri = "/api/tree?x=1".parse().unwrap();
        assert_eq!(log_uri(&uri), "/api/tree?x=1");
    }

    #[test]
    fn protocol_version_header() {
        let mut h = HeaderMap::new();
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        assert_eq!(unsupported_version(&h, &list), None);
        h.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2025-06-18"),
        );
        assert_eq!(unsupported_version(&h, &list), None);
        h.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2026-07-28"),
        );
        assert_eq!(
            unsupported_version(&h, &list).as_deref(),
            Some("2026-07-28")
        );
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        assert_eq!(unsupported_version(&h, &init), None);
        let modern = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
        assert!(unsupported_version(&HeaderMap::new(), &modern).is_some());
        assert_eq!(gen_route("/tree"), Some("tree"));
        assert_eq!(gen_route("/api/graph"), Some("graph"));
        assert_eq!(gen_route("/note"), None);
    }
}
