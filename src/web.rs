//! Embedded web UI (no build step, no CDN — everything ships inside the binary).

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const CSP: &str = "default-src 'self'; img-src 'self' data: blob: https:; media-src 'self' https:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; font-src 'self'; connect-src 'self'; \
frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

macro_rules! asset {
    ($file:literal, $mime:literal) => {
        (
            concat!("/", $file),
            $mime,
            include_bytes!(concat!("../web/", $file)).as_slice(),
        )
    };
}

const ASSETS: &[(&str, &str, &[u8])] = &[
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("app.css", "text/css; charset=utf-8"),
    asset!("icons.js", "text/javascript; charset=utf-8"),
    asset!("graph.js", "text/javascript; charset=utf-8"),
    asset!("i18n.js", "text/javascript; charset=utf-8"),
    asset!("icon.svg", "image/svg+xml"),
    asset!("manifest.webmanifest", "application/manifest+json"),
    asset!("fonts/roboto-flex-latin.woff2", "font/woff2"),
    asset!("fonts/roboto-flex-latin-ext.woff2", "font/woff2"),
];

const INDEX: &[u8] = include_bytes!("../web/index.html");

fn respond(mime: &'static str, body: &'static [u8], cache: &'static str) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    r
}

fn index() -> Response {
    let mut r = respond("text/html; charset=utf-8", INDEX, "no-cache");
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    r
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    let mut r = Router::new().route("/", get(|| async { index() }));
    for (path, mime, body) in ASSETS {
        let cache = if path.starts_with("/fonts/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let (mime, body): (&'static str, &'static [u8]) = (mime, body);
        r = r.route(path, get(move || async move { respond(mime, body, cache) }));
    }
    r
}
