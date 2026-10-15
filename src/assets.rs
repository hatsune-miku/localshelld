//! Vite output is bundled into the executable: no CDN or local development
//! server is needed at runtime, including in the native consent window.
use axum::{
    http::{HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use include_dir::{Dir, include_dir};

static WEB: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

pub fn asset(path: &str) -> Option<(&'static [u8], String)> {
    let path = if path == "/" {
        "index.html"
    } else {
        path.strip_prefix('/')?
    };
    if path.contains("..") || path.contains('\\') {
        return None;
    }
    let file = WEB.get_file(path)?;
    Some((
        file.contents(),
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string(),
    ))
}

pub async fn serve(uri: Uri) -> Response {
    let Some((body, mime)) = asset(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", HeaderValue::from_str(&mime).unwrap());
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}
