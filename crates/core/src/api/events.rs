//! `POST /events`: the event feed as Server-Sent Events (docs/decisions.md,
//! Sync). The stream's life is `crate::events::run`.

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::sse::{KeepAlive, Sse};
use axum::routing::post;
use futures::stream;
use theca_common::api;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};

pub fn router() -> Router<AppState> {
    Router::new().route("/events", post(events))
}

/// The changes to the libraries the credential can access, from its
/// positions, then live: Server-Sent Events. `event: libraries` (data: every
/// library it can access, trash included, each with its position) at the
/// start and whenever that changes; `event: changes` (data: one batch,
/// collapsed per resource, with the new positions); a `:` comment every 30 s.
/// A cut stream resumes from the last positions received.
#[utoipa::path(post, path = "/api/v1/events", request_body = api::Events, responses(
    (status = 200, description = "Server-Sent Events: `libraries` (a list of FeedLibrary), then `changes` (Changes)", content_type = "text/event-stream", body = api::Changes),
))]
pub async fn events(
    axum::extract::State(state): axum::extract::State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::Events>,
) -> Result<axum::response::Response, HttpError> {
    // At most `STREAMS_PER_CREDENTIAL`: one more ends the oldest.
    let registration = state.events.register(user.token, user.account);
    // Small: a client not reading holds the stream back, never fills memory.
    let (out, body) = tokio::sync::mpsc::channel(4);
    let interval = state.events.interval;
    tokio::spawn(crate::events::run(
        state,
        user.0,
        req.after,
        registration,
        out,
    ));
    let body = stream::unfold(body, |mut body| async move {
        body.recv()
            .await
            .map(|event| (Ok::<_, std::convert::Infallible>(event), body))
    });
    Ok((
        // nginx buffers responses unless told not to.
        [
            ("x-accel-buffering", "no"),
            (header::CACHE_CONTROL.as_str(), "no-cache"),
        ],
        Sse::new(body).keep_alive(KeepAlive::new().interval(interval)),
    )
        .into_response())
}
