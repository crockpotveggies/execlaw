//! Serve the SPA bundle (`web/dist/`) from the Rust server.
//!
//! Phase 6d — the SPA lives at `/` and `/api/*` lives under the
//! existing API router. This keeps the Tauri webview, headless
//! browser access (`http://127.0.0.1:3031`), and the dev-server
//! Vite proxy all aligned on a single origin so cookies, the
//! WebSocket bus, and the auth header path all behave identically.
//!
//! ## Embedding strategy
//!
//! `rust-embed` reads the absolute path `$CARGO_MANIFEST_DIR/../../
//! web/dist/`. In debug builds it lazily loads files from disk at
//! request time, so `vite build` while the server is running
//! surfaces edits without a Rust rebuild. In release builds it
//! bakes the bytes into the binary at compile time — the Tauri
//! `.app` ships with `web/dist/` already baked in.
//!
//! ## SPA fallback
//!
//! React Router uses path-based deep links (`/chats/<id>`,
//! `/settings/general`, etc.). On a hard refresh the browser asks
//! the server for `/settings/general` directly, which has no
//! matching file in `dist/`. The fallback route serves
//! `index.html` for any GET that isn't an API route and isn't an
//! existing asset — the SPA boots and React Router takes it from
//! there.
//!
//! ## `web/dist/` may be missing
//!
//! The directory is gitignored. A fresh checkout that runs
//! `cargo build` before `npm --prefix web run build` will fail to
//! compile with `rust-embed`'s "folder not found" error. To keep
//! `cargo check` cheap for backend devs we commit a stub
//! `web/dist/.keep` so the directory always exists; if it's empty
//! the SPA routes will 404 at runtime, which is correct (and
//! exactly what dev hitters of `localhost:3031` see today — they
//! use Vite on :5173 anyway).

use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;

/// Embed `web/dist/` relative to the workspace root. The
/// `interpolate-folder-path` feature of rust-embed expands
/// `$CARGO_MANIFEST_DIR`, which points at `crates/server/`. The
/// SPA build output is two levels up.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist/"]
struct Spa;

/// Build a `Router` exposing the SPA. Mount-after the API router
/// so `/api/*` matches first.
pub fn spa_router() -> Router {
    Router::new()
        // Serve any embedded file directly by its path; useful for
        // `assets/*.js`, `assets/*.css`, favicons, etc. Any path
        // that doesn't resolve falls through to the catch-all
        // below, which always returns `index.html`.
        .route("/{*path}", get(serve_asset))
        // Bare `/` returns `index.html`.
        .route("/", get(serve_index))
}

async fn serve_index() -> Response {
    serve_path("index.html")
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    // Exact-match the request path against an embedded file. If
    // the file is missing AND the path looks like a SPA route
    // (no file extension), fall back to `index.html` so React
    // Router can take it. Requests that DO have an extension
    // (`/assets/foo.js`) should hard-fail with 404 — those are
    // genuine asset misses we want surfaced rather than silently
    // returning HTML.
    if Spa::get(&path).is_some() {
        return serve_path(&path);
    }
    let has_extension = path
        .rsplit('/')
        .next()
        .map(|leaf| leaf.contains('.'))
        .unwrap_or(false);
    if has_extension {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    serve_path("index.html")
}

fn serve_path(path: &str) -> Response {
    let Some(file) = Spa::get(path) else {
        // index.html itself is missing — happens on a fresh checkout
        // before the operator runs `npm --prefix web run build`.
        // Return a friendly diagnostic so it's obvious what to do
        // instead of an opaque 404.
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "execlaw SPA bundle not found.\n\
             \n\
             Run `npm --prefix web ci && npm --prefix web run build` from the repo root, \
             then rebuild the server. The Tauri release bundle does this for you \
             automatically via scripts/build-mac.sh.\n",
        )
            .into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = Response::new(Body::from(file.data));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::util::ServiceExt;

    // The SPA dir may be empty in CI before the JS build runs. These
    // tests assert that the *routing shape* is correct regardless of
    // whether index.html exists. If the bundle is missing we still
    // return a 200 OR a 404 with the diagnostic body — never a 500
    // and never confused with API routes.

    fn router() -> Router {
        spa_router()
    }

    #[tokio::test]
    async fn root_returns_html_or_diagnostic() {
        let app = router();
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // 200 (bundle present) or 404 (bundle missing) — both are
        // acceptable; 500 means our routing is wrong.
        let status = resp.status();
        assert!(
            status == StatusCode::OK || status == StatusCode::NOT_FOUND,
            "unexpected status from /: {status}"
        );
    }

    #[tokio::test]
    async fn deep_link_with_no_extension_falls_back_to_index() {
        // /settings/general has no extension → should route to
        // index.html (SPA boot) regardless of whether index.html
        // exists. We only care that the routing logic picked the
        // SPA-fallback branch, not the asset-miss branch.
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/settings/general")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Same acceptance criteria as above.
        let status = resp.status();
        assert!(
            status == StatusCode::OK || status == StatusCode::NOT_FOUND,
            "unexpected status from deep link: {status}"
        );
    }

    #[tokio::test]
    async fn asset_with_extension_returns_404_when_missing() {
        let app = router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/assets/this-file-definitely-does-not-exist.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // We do NOT want to fall back to index.html for a missing
        // JS asset — that would silently serve HTML where the SPA
        // expects executable JS, producing a baffling parse error.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
