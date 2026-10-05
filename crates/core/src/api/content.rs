//! Serving a file's content to the browser (previews): `<img>`, `<video>`,
//! `<iframe>` with the session cookie, `Range` supported.
//!
//! User content on the app's origin could run script with the viewer's session
//! (an HTML or SVG file): every response is sandboxed (`CSP: sandbox`) and
//! `nosniff`, and types that carry script are only ever served as plain text.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::HttpError;
use crate::db;
use crate::stream::{VersionReader, parse_range};

#[derive(Deserialize, utoipa::IntoParams)]
pub(super) struct ContentParams {
    /// A version of the file (default: the current one).
    version: Option<Uuid>,
}

/// A file's content, streamed (previews and downloads in the browser).
#[utoipa::path(get, path = "/api/v1/files/{id}/content", params(("id" = Uuid, Path), ContentParams), responses(
    (status = 200, description = "The whole file"),
    (status = 206, description = "The requested range"),
    (status = 404, body = theca_common::api::ApiError),
    (status = 416, description = "Range not satisfiable"),
))]
pub async fn content(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    Query(params): Query<ContentParams>,
    request: HeaderMap,
) -> Result<Response, HttpError> {
    let node = db::library::node_access(&state.pool, &user, id)
        .await?
        .filter(|n| n.is_file)
        .ok_or_else(|| HttpError::not_found("file"))?;
    let version = match params.version {
        Some(v) => db::node::version_of(&state.pool, node.node, v).await?,
        None => node.current_version,
    }
    .ok_or_else(|| HttpError::not_found("version"))?;
    let info = db::node::info(&state.pool, node.node).await?;
    let etag = format!(
        "\"{}\"",
        params.version.or(info.current_version).unwrap_or_default()
    );
    let reader = VersionReader::open(&state, version)
        .await
        .map_err(HttpError::internal)?;
    let len = reader.len();

    let (content_type, inline) = content_type(&info.name);
    let mut headers = HeaderMap::new();
    let value = |v: &str| HeaderValue::from_str(v).expect("ASCII header");
    headers.insert(header::CONTENT_TYPE, value(content_type));
    headers.insert(header::ACCEPT_RANGES, value("bytes"));
    headers.insert(header::ETAG, value(&etag));
    headers.insert(header::CACHE_CONTROL, value("private, no-cache"));
    headers.insert(header::CONTENT_SECURITY_POLICY, value("sandbox"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, value("nosniff"));
    let disposition = if inline { "inline" } else { "attachment" };
    headers.insert(
        header::CONTENT_DISPOSITION,
        value(&format!(
            "{disposition}; filename*=UTF-8''{}",
            percent_encode(&info.name)
        )),
    );

    let range = request.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (status, start, end) = match parse_range(range, len) {
        None => (StatusCode::OK, 0, len),
        Some(Ok((start, end))) => {
            headers.insert(
                header::CONTENT_RANGE,
                value(&format!("bytes {start}-{}/{len}", end - 1)),
            );
            (StatusCode::PARTIAL_CONTENT, start, end)
        }
        Some(Err(())) => {
            headers.insert(header::CONTENT_RANGE, value(&format!("bytes */{len}")));
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    headers.insert(header::CONTENT_LENGTH, value(&(end - start).to_string()));
    Ok((
        status,
        headers,
        Body::from_stream(reader.into_stream(start, end)),
    )
        .into_response())
}

/// The type to serve a file as, from its extension, and whether the browser
/// may show it (`inline`) or should download it. Anything that can carry
/// script (HTML, SVG, XML, JavaScript) is plain text; unknown types download.
pub fn content_type(name: &str) -> (&'static str, bool) {
    let extension = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" => ("application/pdf", true),
        "png" => ("image/png", true),
        "jpg" | "jpeg" => ("image/jpeg", true),
        "gif" => ("image/gif", true),
        "webp" => ("image/webp", true),
        "avif" => ("image/avif", true),
        "mp4" | "m4v" => ("video/mp4", true),
        "webm" => ("video/webm", true),
        "mov" => ("video/quicktime", true),
        "mp3" => ("audio/mpeg", true),
        "m4a" => ("audio/mp4", true),
        "ogg" | "oga" => ("audio/ogg", true),
        "wav" => ("audio/wav", true),
        "flac" => ("audio/flac", true),
        "txt" | "md" | "csv" | "log" | "json" | "yaml" | "yml" | "toml" | "rs" | "py" | "sh"
        | "html" | "htm" | "svg" | "xml" | "xhtml" | "js" | "mjs" | "css" => {
            ("text/plain; charset=utf-8", true)
        }
        _ => ("application/octet-stream", false),
    }
}

/// RFC 5987 percent-encoding for `filename*`.
fn percent_encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::content_type;

    #[test]
    fn script_types_are_never_rendered() {
        for name in ["a.html", "a.HTM", "a.svg", "a.xml", "a.js"] {
            assert_eq!(content_type(name).0, "text/plain; charset=utf-8", "{name}");
        }
        assert_eq!(content_type("movie.MP4"), ("video/mp4", true));
        assert_eq!(
            content_type("archive.zip"),
            ("application/octet-stream", false)
        );
        assert_eq!(
            content_type("noextension"),
            ("application/octet-stream", false)
        );
    }
}
