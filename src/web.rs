//! Embedded web UI (no build step, no CDN — everything ships inside the binary).

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::RustEmbed;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

const CSP: &str = "default-src 'self'; img-src 'self' data: blob: https:; media-src 'self' https:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; font-src 'self' data:; connect-src 'self'; \
frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Changes whenever any embedded asset changes, so the service worker refreshes its cache.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    use sha2::{Digest, Sha256};
    let mut names: Vec<_> = Assets::iter().collect();
    names.sort();
    let mut h = Sha256::new();
    for n in names {
        h.update(n.as_bytes());
        if let Some(f) = Assets::get(&n) {
            h.update(f.metadata.sha256_hash());
        }
    }
    hex::encode(&h.finalize()[..6])
});

static SW_BODY: LazyLock<String> = LazyLock::new(|| {
    let sw = Assets::get("sw.js")
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())
        .unwrap_or_default();
    sw.replace("__VERSION__", &ASSET_VERSION)
});

fn mime_for(path: &str) -> &'static str {
    let p = path.trim_end_matches(".gz");
    match p.rsplit('.').next().unwrap_or("") {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "webmanifest" => "application/manifest+json",
        "woff2" => "font/woff2",
        "json" => "application/json",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

fn not_modified(req: &HeaderMap, tag: &HeaderValue) -> bool {
    req.get(header::IF_NONE_MATCH).is_some_and(|v| {
        v.as_bytes()
            .split(|b| *b == b',')
            .any(|t| t.trim_ascii() == tag.as_bytes())
    })
}

fn asset(path: &str, req: &HeaderMap) -> Response {
    // Prefer a pre-compressed copy (served as gzip, which every browser accepts).
    let (file, gz) = match Assets::get(&format!("{path}.gz")) {
        Some(f) => (f, true),
        None => match Assets::get(path) {
            Some(f) => (f, false),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let tag = HeaderValue::from_str(&format!(
        "\"{}\"",
        hex::encode(&file.metadata.sha256_hash()[..12])
    ))
    .expect("hex etag");
    let long = path.starts_with("fonts/") || path.starts_with("vendor/");
    let cache = HeaderValue::from_static(if long {
        "public, max-age=604800"
    } else {
        "no-cache"
    });
    if not_modified(req, &tag) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, tag);
        r.headers_mut().insert(header::CACHE_CONTROL, cache);
        return r;
    }
    let mut r = Response::new(Body::from(file.data));
    let h = r.headers_mut();
    h.insert(header::ETAG, tag);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime_for(path)),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, cache);
    if gz {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    r
}

fn index(req: &HeaderMap) -> Response {
    let mut r = asset("index.html", req);
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
    Router::new()
        .route("/", get(|h: HeaderMap| async move { index(&h) }))
        .route(
            "/sw.js",
            get(|req: HeaderMap| async move {
                let tag =
                    HeaderValue::from_str(&format!("\"sw-{}\"", *ASSET_VERSION)).expect("etag");
                if not_modified(&req, &tag) {
                    let mut r = StatusCode::NOT_MODIFIED.into_response();
                    r.headers_mut().insert(header::ETAG, tag);
                    return r;
                }
                let mut r = Response::new(Body::from(SW_BODY.as_str()));
                let h = r.headers_mut();
                h.insert(header::ETAG, tag);
                h.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/javascript; charset=utf-8"),
                );
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                h.insert("service-worker-allowed", HeaderValue::from_static("/"));
                r
            }),
        )
        // Share-target fallback when the service worker is not active yet: just open the app.
        .route(
            "/share",
            post(|| async {
                (
                    StatusCode::SEE_OTHER,
                    [(header::LOCATION, "/#/share-failed")],
                )
            }),
        )
        .route(
            "/{*path}",
            get(|Path(p): Path<String>, h: HeaderMap| async move {
                if p.contains("..") || p == "sw.js" || p == "index.html" {
                    return StatusCode::NOT_FOUND.into_response();
                }
                asset(&p, &h)
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_present() {
        for p in [
            "index.html",
            "app.js",
            "app.css",
            "sw.js",
            "vendor/katex/katex.min.js",
            "vendor/mermaid.min.js.gz",
        ] {
            assert!(Assets::get(p).is_some(), "{p} missing");
        }
        assert!(!SW_BODY.contains("__VERSION__"));
        assert_eq!(ASSET_VERSION.len(), 12);
    }
}
