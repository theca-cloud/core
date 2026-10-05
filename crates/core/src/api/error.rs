//! API errors: an HTTP status plus the shared [`ApiError`] body. Database
//! refusals are mapped here, in one place, from their constraint names.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use theca_common::api::ApiError;
use uuid::Uuid;

/// An error returned by a handler.
#[derive(Debug)]
pub struct HttpError {
    status: StatusCode,
    body: ApiError,
}

impl HttpError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError {
                code: code.into(),
                message: message.into(),
                chunks: vec![],
            },
        }
    }

    /// Replaces the error code (e.g. a `forbidden` with a more specific code).
    pub fn into_code(mut self, code: &str) -> Self {
        self.body.code = code.into();
        self
    }

    pub fn with_chunks(mut self, chunks: Vec<Uuid>) -> Self {
        self.body.chunks = chunks;
        self
    }

    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "missing or unknown user",
        )
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }

    pub fn bad_request(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        tracing::error!("{message}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error, see the server logs",
        )
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

/// Database errors: named constraints become API errors, anything else is internal.
impl From<sqlx::Error> for HttpError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(e) = &error {
            match e.constraint() {
                Some("node_name") => {
                    return Self::conflict("name_taken", "a file or folder with this name exists");
                }
                Some("node_no_cycle") => {
                    return Self::conflict("move_into_itself", "a folder can't move into itself");
                }
                Some("personal_organization") => {
                    return Self::conflict(
                        "personal_organization",
                        "a personal organization has no other member",
                    );
                }
                Some("library_name") => {
                    return Self::conflict("library_name_taken", "a library with this name exists");
                }
                Some("node_name_valid" | "library_name_valid") => {
                    return Self::bad_request("name_invalid", "invalid name");
                }
                Some("chunk_plain_size") => {
                    return Self::bad_request(
                        "invalid_plain_size",
                        "a chunk's plain size is at most 8 MiB",
                    );
                }
                Some("chunk_in_version_chunk") => {
                    return Self::conflict(
                        "chunk_missing",
                        "a chunk is not uploaded (pending) or was deleted",
                    );
                }
                _ => {}
            }
        }
        Self::internal(error)
    }
}

impl From<crate::storage::BackendError> for HttpError {
    fn from(error: crate::storage::BackendError) -> Self {
        Self::internal(error)
    }
}

/// `Json` whose refusals (malformed body, invalid name, ...) are API errors too,
/// so clients only ever get one error format.
pub struct ApiJson<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = HttpError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, HttpError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(HttpError::bad_request(
                "invalid_request",
                rejection.body_text(),
            )),
        }
    }
}
