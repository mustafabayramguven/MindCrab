//! HTTP server: web UI, REST API (`/api/*`) and MCP over Streamable HTTP (`/mcp`).

use crate::app::{AResult, App, AppError};
use crate::mcp::{self, Session};
use crate::scope::Scope;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

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
        .layer(middleware::from_fn_with_state(shared.clone(), require_auth))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/health", get(|| async { "ok" }));

    Router::new()
        .nest("/api", api)
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .route(
            "/mcp/{token}",
            post(mcp_post_path).get(mcp_get).delete(mcp_delete),
        )
        .merge(crate::web::routes())
        .layer(DefaultBodyLimit::max(
            crate::vault::MAX_FILE_BYTES + 1024 * 1024,
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(shared)
}

pub fn shared(app: Arc<App>, cfg: Config) -> Arc<Shared> {
    Arc::new(Shared {
        app,
        cfg,
        sessions: Mutex::new(HashMap::new()),
    })
}

async fn require_auth(State(st): St, jar: CookieJar, req: Request, next: Next) -> Response {
    let ok = bearer(req.headers()).is_some_and(|t| eq(t, &st.cfg.web_token))
        || jar
            .get(COOKIE)
            .is_some_and(|c| eq(c.value(), &st.cfg.web_token));
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "login required".into()).into_response();
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct LoginReq {
    token: String,
}

async fn login(State(st): St, jar: CookieJar, Json(r): Json<LoginReq>) -> Response {
    if !eq(r.token.trim(), &st.cfg.web_token) {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        return ApiError(StatusCode::UNAUTHORIZED, "wrong token".into()).into_response();
    }
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

async fn file(State(st): St, Path(path): Path<String>) -> ApiResult<Response> {
    let p = path.clone();
    let bytes = run(&st, move |app| app.read_file(&p)).await?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let mut resp = Response::new(Body::from(bytes));
    let h = resp.headers_mut();
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

async fn mcp_post(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let scope = match bearer(&headers) {
        Some(t) => resolve_scope(&st, t).await,
        None => None,
    };
    let Some(scope) = scope else {
        let mut r = (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid or missing bearer token"})),
        )
            .into_response();
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"mindcrab\""),
        );
        return r;
    };
    mcp_dispatch(st, headers, body, scope).await
}

async fn mcp_post_path(
    State(st): St,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(scope) = resolve_scope(&st, &token).await else {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    mcp_dispatch(st, headers, body, scope).await
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

async fn mcp_delete(State(st): St, headers: HeaderMap) -> StatusCode {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|h| h.to_str().ok()) {
        st.sessions.lock().unwrap().remove(sid);
    }
    StatusCode::OK
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
