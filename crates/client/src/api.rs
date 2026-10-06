//! The API, over the `common` types: one generic [`Client::call`], and a typed
//! method per endpoint the engines and the clients use.

use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use theca_common::api;
use uuid::Uuid;

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server refused, with its error (`code` is stable).
    #[error("{} ({}, HTTP {status})", error.message, error.code)]
    Api { status: u16, error: api::ApiError },
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected answer: {0}")]
    Decode(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("this server doesn't speak API v1")]
    Version,
    #[error("cancelled")]
    Cancelled,
    #[error("the file changed during the upload")]
    SourceChanged,
    #[error("chunk {0} is corrupted (wrong hash or undecodable)")]
    Corrupted(Uuid),
    /// The storage refused a presigned upload or download.
    #[error("the storage answered HTTP {status} for chunk {chunk}")]
    Storage { chunk: Uuid, status: u16 },
}

impl Error {
    /// The server's error code (`name_taken`, `chunk_missing`, ...), if any.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { error, .. } => Some(&error.code),
            _ => None,
        }
    }

    /// Worth retrying: the network, or the server's or storage's 5xx.
    pub(crate) fn transient(&self) -> bool {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Error::Http(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            // The browser's fetch doesn't tell a connection error apart.
            #[cfg(target_arch = "wasm32")]
            Error::Http(e) => e.is_timeout() || e.is_request(),
            Error::Api { status, .. } | Error::Storage { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// A connection to a server.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    /// `Bearer` token (an app password or a session); `None` in the browser,
    /// where the session cookie goes along by itself.
    token: Option<String>,
}

impl Client {
    /// A client for the server at `base` (e.g. `https://cloud.example.com`).
    pub fn new(base: &str) -> Result<Self, Error> {
        #[cfg(not(target_arch = "wasm32"))]
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            base: base.trim_end_matches('/').to_owned(),
            token: None,
        })
    }

    /// Authenticates with a token (an app password, or a session's).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// The token in use, if any (e.g. the session a login opened).
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.http.request(method, format!("{}{path}", self.base));
        #[cfg(target_arch = "wasm32")]
        let request = request.fetch_credentials_include();
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Any endpoint: `path` from `/api/...`, an optional JSON body, a JSON
    /// answer (`()` for empty answers).
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T, Error> {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            let json = serde_json::to_vec(body).map_err(|e| Error::Decode(e.to_string()))?;
            request = request
                .header("content-type", "application/json")
                .body(json);
        }
        let response = request.send().await?;
        decode(response).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.call(Method::GET, path, None::<&()>).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T, Error> {
        self.call(Method::POST, path, Some(body)).await
    }

    /// Checks the server speaks API v1.
    pub async fn check_version(&self) -> Result<(), Error> {
        #[derive(serde::Deserialize)]
        struct Version {
            api: Vec<String>,
        }
        let version: Version = self.get("/api/version").await?;
        if version.api.iter().any(|v| v == "v1") {
            Ok(())
        } else {
            Err(Error::Version)
        }
    }

    /// Logs in with a password (and a TOTP code if on). Natively, the session
    /// token becomes this client's `Bearer` token; in the browser, the cookie.
    pub async fn login(
        &mut self,
        email: &str,
        password: &str,
        totp: Option<&str>,
    ) -> Result<api::Me, Error> {
        let body = api::Login {
            email: email.to_owned(),
            password: password.to_owned(),
            totp: totp.map(str::to_owned),
        };
        let response = self
            .request(Method::POST, "/api/v1/auth/login")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).map_err(|e| Error::Decode(e.to_string()))?)
            .send()
            .await?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(token) = session_cookie(response.headers()) {
            self.token = Some(token);
        }
        decode(response).await
    }

    /// Sets up a fresh instance (logged in as its admin afterwards).
    pub async fn setup(
        &mut self,
        setup: &api::Setup,
        token: Option<&str>,
    ) -> Result<api::Me, Error> {
        let mut request = self
            .request(Method::POST, "/api/v1/setup")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(setup).map_err(|e| Error::Decode(e.to_string()))?);
        if let Some(token) = token {
            request = request.header("x-theca-setup-token", token);
        }
        let response = request.send().await?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(token) = session_cookie(response.headers()) {
            self.token = Some(token);
        }
        decode(response).await
    }

    pub async fn me(&self) -> Result<api::Me, Error> {
        self.get("/api/v1/me").await
    }

    /// Revokes the session in use (`not_a_session` with an app password).
    pub async fn logout(&self) -> Result<(), Error> {
        self.call(Method::POST, "/api/v1/auth/logout", None::<&()>)
            .await
    }

    pub async fn libraries(&self) -> Result<Vec<api::Library>, Error> {
        self.get("/api/v1/libraries").await
    }

    pub async fn create_library(
        &self,
        organization: Uuid,
        name: &str,
    ) -> Result<api::Library, Error> {
        let body = serde_json::json!({ "name": name, "organization": organization });
        self.post("/api/v1/libraries", &body).await
    }

    pub async fn children(&self, folder: Uuid) -> Result<Vec<api::Node>, Error> {
        self.get(&format!("/api/v1/nodes/{folder}/children")).await
    }

    pub async fn create_folder(&self, parent: Uuid, name: &str) -> Result<api::Node, Error> {
        self.post(
            &format!("/api/v1/nodes/{parent}/folders"),
            &serde_json::json!({ "name": name }),
        )
        .await
    }

    pub async fn create_app_password(
        &self,
        request: &api::CreateAppPassword,
    ) -> Result<api::NewAppPassword, Error> {
        self.post("/api/v1/auth/app-passwords", request).await
    }

    pub async fn versions(&self, file: Uuid) -> Result<Vec<api::Version>, Error> {
        self.get(&format!("/api/v1/files/{file}/versions")).await
    }

    /// Makes a version current.
    pub async fn restore_version(&self, file: Uuid, version: Uuid) -> Result<(), Error> {
        self.post(
            &format!("/api/v1/files/{file}/versions/{version}/restore"),
            &(),
        )
        .await
    }

    /// A file or folder, with its library and path.
    pub async fn node(&self, id: Uuid) -> Result<api::NodeInfo, Error> {
        self.get(&format!("/api/v1/nodes/{id}")).await
    }

    /// Renames and/or moves a node (`replace`: merge into a file of that name).
    pub async fn update_node(
        &self,
        id: Uuid,
        update: &api::UpdateNode,
    ) -> Result<api::NodeInfo, Error> {
        self.call(Method::PATCH, &format!("/api/v1/nodes/{id}"), Some(update))
            .await
    }

    /// Copies a node (`replace`: a new version of a file of that name).
    pub async fn copy_node(&self, id: Uuid, copy: &api::CopyNode) -> Result<api::NodeInfo, Error> {
        self.post(&format!("/api/v1/nodes/{id}/copy"), copy).await
    }

    /// Puts a node in the trash, or (`permanent`, from the trash) deletes it
    /// for good.
    pub async fn delete_node(&self, id: Uuid, permanent: bool) -> Result<(), Error> {
        self.call(
            Method::DELETE,
            &format!("/api/v1/nodes/{id}?permanent={permanent}"),
            None::<&()>,
        )
        .await
    }

    /// Takes a node out of the trash.
    pub async fn restore_node(&self, id: Uuid) -> Result<api::Restored, Error> {
        self.post(&format!("/api/v1/nodes/{id}/restore"), &()).await
    }

    /// A library's trash.
    pub async fn library_trash(&self, library: Uuid) -> Result<Vec<api::TrashEntry>, Error> {
        self.get(&format!("/api/v1/libraries/{library}/trash"))
            .await
    }

    pub async fn rename_library(&self, library: Uuid, name: &str) -> Result<api::Library, Error> {
        let body = serde_json::json!({ "name": name });
        self.call(
            Method::PATCH,
            &format!("/api/v1/libraries/{library}"),
            Some(&body),
        )
        .await
    }

    /// Puts a library in the trash, or (`permanent`, from the trash) deletes it
    /// for good.
    pub async fn delete_library(&self, library: Uuid, permanent: bool) -> Result<(), Error> {
        self.call(
            Method::DELETE,
            &format!("/api/v1/libraries/{library}?permanent={permanent}"),
            None::<&()>,
        )
        .await
    }

    pub async fn restore_library(&self, library: Uuid) -> Result<api::Library, Error> {
        self.post(&format!("/api/v1/libraries/{library}/restore"), &())
            .await
    }

    /// An organization's trashed libraries.
    pub async fn organization_trash(
        &self,
        organization: Uuid,
    ) -> Result<Vec<api::TrashedLibrary>, Error> {
        self.get(&format!("/api/v1/organizations/{organization}/trash"))
            .await
    }

    /// Names containing `text` (at least 3 characters), in one library or all.
    pub async fn search(
        &self,
        text: &str,
        library: Option<Uuid>,
    ) -> Result<Vec<api::SearchResult>, Error> {
        let mut url = reqwest::Url::parse("http://x/api/v1/search").expect("a valid URL");
        url.query_pairs_mut().append_pair("q", text);
        if let Some(library) = library {
            url.query_pairs_mut()
                .append_pair("library", &library.to_string());
        }
        let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
        self.get(&path).await
    }

    pub async fn app_passwords(&self) -> Result<Vec<api::AppPassword>, Error> {
        self.get("/api/v1/auth/app-passwords").await
    }

    pub async fn revoke_app_password(&self, id: Uuid) -> Result<(), Error> {
        self.call(
            Method::DELETE,
            &format!("/api/v1/auth/app-passwords/{id}"),
            None::<&()>,
        )
        .await
    }

    pub async fn lookup(&self, lookup: &api::Lookup) -> Result<Vec<api::LookupEntry>, Error> {
        self.post("/api/v1/chunks/lookup", lookup).await
    }

    pub async fn upload_urls(
        &self,
        declarations: &[api::Declare],
    ) -> Result<Vec<api::ChunkUrl>, Error> {
        self.post("/api/v1/chunks/upload-urls", declarations).await
    }

    /// Returns the chunks not uploaded yet.
    pub async fn announce(&self, chunks: &[Uuid]) -> Result<Vec<Uuid>, Error> {
        let answer: api::Chunks = self
            .post(
                "/api/v1/chunks/announce",
                &api::Chunks {
                    chunks: chunks.to_vec(),
                },
            )
            .await?;
        Ok(answer.chunks)
    }

    pub async fn cancel(&self, chunks: &[Uuid]) -> Result<(), Error> {
        self.post(
            "/api/v1/chunks/cancel",
            &api::Chunks {
                chunks: chunks.to_vec(),
            },
        )
        .await
    }

    pub async fn commit(&self, commit: &api::Commit) -> Result<api::Committed, Error> {
        self.post("/api/v1/files/commit", commit).await
    }

    /// A version's chunks with their presigned URLs (the current one by default).
    pub async fn download_manifest(
        &self,
        file: Uuid,
        version: Option<Uuid>,
    ) -> Result<api::Download, Error> {
        match version {
            Some(v) => {
                self.get(&format!("/api/v1/files/{file}/download?version={v}"))
                    .await
            }
            None => self.get(&format!("/api/v1/files/{file}/download")).await,
        }
    }

    /// The raw HTTP client, for presigned URLs (no `Authorization`: the URL is
    /// the credential).
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let error = serde_json::from_slice(&bytes).unwrap_or_else(|_| api::ApiError {
            code: "http".into(),
            message: String::from_utf8_lossy(&bytes).into_owned(),
            chunks: Vec::new(),
        });
        return Err(Error::Api {
            status: status.as_u16(),
            error,
        });
    }
    let bytes = if bytes.is_empty() || status == StatusCode::NO_CONTENT {
        &b"null"[..]
    } else {
        &bytes[..]
    };
    serde_json::from_slice(bytes).map_err(|e| Error::Decode(e.to_string()))
}

/// The session token a login sets as a cookie.
#[cfg(not(target_arch = "wasm32"))]
fn session_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            v.strip_prefix("theca_session=")?
                .split(';')
                .next()
                .map(str::to_owned)
        })
        .filter(|t| !t.is_empty())
}
