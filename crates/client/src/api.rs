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
    /// A name the server would refuse (checked before asking it).
    #[error("invalid name: {0}")]
    InvalidName(String),
    /// A file goes where a folder has its name.
    #[error("{0}: a folder has this name")]
    FolderExists(String),
}

impl Error {
    /// A stable code for every error, as the API's (`name_taken`,
    /// `quota_exceeded`, ...) when the server refused, the client's otherwise:
    /// what a UI shows its message by.
    pub fn kind(&self) -> &str {
        match self {
            Error::Api { error, .. } => &error.code,
            Error::Http(_) => "network",
            Error::Decode(_) => "unexpected_answer",
            Error::Io(_) => "io",
            Error::Version => "unsupported_server",
            Error::Cancelled => "cancelled",
            Error::SourceChanged => "source_changed",
            Error::Corrupted(_) => "corrupted",
            Error::Storage { .. } => "storage",
            Error::InvalidName(_) => "invalid_name",
            Error::FolderExists(_) => "folder_exists",
        }
    }

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

    /// The instance's limits (they change live).
    pub async fn limits(&self) -> Result<api::Limits, Error> {
        self.get("/api/v1/limits").await
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

    /// Folders below `parent`, like `mkdir -p` (`a`, `a/b`): each path's
    /// folder, in order, created or existing (at most 1000 paths).
    pub async fn create_folders(
        &self,
        parent: Uuid,
        paths: &[String],
    ) -> Result<Vec<api::FolderPath>, Error> {
        self.post(
            &format!("/api/v1/nodes/{parent}/folders"),
            &api::CreateFolders {
                paths: paths.to_vec(),
            },
        )
        .await
    }

    /// A folder's whole subtree, as it streams in: its nodes, parents before
    /// their children. A stream cut before its end line ends with an error,
    /// never passes for a complete listing.
    pub async fn tree(
        &self,
        folder: Uuid,
    ) -> Result<impl futures::Stream<Item = Result<api::TreeNode, Error>> + use<>, Error> {
        let response = self
            .request(Method::GET, &format!("/api/v1/nodes/{folder}/tree"))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(decode::<serde_json::Value>(response)
                .await
                .err()
                .unwrap_or(Error::Decode("an error without an error".into())));
        }
        Ok(tree_lines(response.bytes_stream()))
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

    /// Renames and/or moves nodes (`replace`: merge into a file of that name),
    /// each in its own transaction (at most 1000): each one's outcome, in order.
    pub async fn update_nodes(
        &self,
        nodes: &[api::UpdateNode],
    ) -> Result<Vec<Result<api::NodeInfo, Error>>, Error> {
        let body = api::UpdateNodes {
            nodes: nodes.to_vec(),
        };
        let outcomes: Vec<api::NodeOutcome> = self
            .call(Method::PATCH, "/api/v1/nodes", Some(&body))
            .await?;
        Ok(outcomes.into_iter().map(node_outcome).collect())
    }

    /// Copies nodes (`replace`: a new version of a file of that name), each in
    /// its own transaction (at most 1000): each copy's outcome, in order.
    pub async fn copy_nodes(
        &self,
        nodes: &[api::CopyNode],
    ) -> Result<Vec<Result<api::NodeInfo, Error>>, Error> {
        let body = api::CopyNodes {
            nodes: nodes.to_vec(),
        };
        let outcomes: Vec<api::NodeOutcome> = self.post("/api/v1/nodes/copy", &body).await?;
        Ok(outcomes.into_iter().map(node_outcome).collect())
    }

    /// Puts nodes in the trash, or (`permanent`, from the trash) deletes them
    /// for good, each in its own transaction (at most 1000): each one's
    /// outcome, in order.
    pub async fn trash_nodes(
        &self,
        nodes: &[Uuid],
        permanent: bool,
    ) -> Result<Vec<Result<(), Error>>, Error> {
        let body = api::TrashNodes {
            nodes: nodes.to_vec(),
            permanent,
        };
        let outcomes: Vec<api::NodeOutcome> = self.post("/api/v1/nodes/trash", &body).await?;
        Ok(outcomes
            .into_iter()
            .map(|o| match o.error {
                Some(error) => Err(Error::Api {
                    status: o.status.unwrap_or(409),
                    error,
                }),
                None => Ok(()),
            })
            .collect())
    }

    /// Takes nodes out of the trash, each in its own transaction (at most
    /// 1000): each one's outcome, in order.
    pub async fn restore_nodes(
        &self,
        nodes: &[Uuid],
    ) -> Result<Vec<Result<api::Restored, Error>>, Error> {
        let body = api::RestoreNodes {
            nodes: nodes.to_vec(),
        };
        let outcomes: Vec<api::RestoreOutcome> = self.post("/api/v1/nodes/restore", &body).await?;
        Ok(outcomes
            .into_iter()
            .map(|o| match (o.restored, o.error) {
                (Some(restored), _) => Ok(restored),
                (None, Some(error)) => Err(Error::Api {
                    status: o.status.unwrap_or(409),
                    error,
                }),
                (None, None) => Err(Error::Decode("an empty restore outcome".into())),
            })
            .collect())
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

    /// Several commits, each in its own transaction (at most 100): each one's
    /// outcome, in order.
    pub async fn commit(
        &self,
        commits: &[api::Commit],
    ) -> Result<Vec<Result<api::Committed, Error>>, Error> {
        let outcomes: Vec<api::CommitOutcome> = self
            .post(
                "/api/v1/files/commit",
                &api::Commits {
                    commits: commits.to_vec(),
                },
            )
            .await?;
        Ok(outcomes
            .into_iter()
            .map(|o| match (o.committed, o.error) {
                (Some(committed), _) => Ok(committed),
                (None, Some(error)) => Err(Error::Api {
                    status: o.status.unwrap_or(409),
                    error,
                }),
                (None, None) => Err(Error::Decode("an empty commit outcome".into())),
            })
            .collect())
    }

    /// Files' chunks with their presigned URLs (each file's current version,
    /// or the one asked; at most 100): each one's outcome, in order.
    pub async fn manifests(
        &self,
        files: &[api::DownloadFile],
    ) -> Result<Vec<Result<api::Download, Error>>, Error> {
        let outcomes: Vec<api::DownloadOutcome> = self
            .post(
                "/api/v1/files/download",
                &api::Downloads {
                    files: files.to_vec(),
                },
            )
            .await?;
        Ok(outcomes
            .into_iter()
            .map(|o| match (o.download, o.error) {
                (Some(download), _) => Ok(download),
                (None, Some(error)) => Err(Error::Api {
                    status: o.status.unwrap_or(404),
                    error,
                }),
                (None, None) => Err(Error::Decode("an empty download outcome".into())),
            })
            .collect())
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

/// NDJSON tree lines into nodes, checking the end line (and its count).
fn tree_lines<B>(bytes: B) -> impl futures::Stream<Item = Result<api::TreeNode, Error>>
where
    B: futures::Stream<Item = reqwest::Result<bytes::Bytes>>,
{
    use futures::StreamExt;
    struct Lines<B> {
        bytes: std::pin::Pin<Box<B>>,
        buffer: Vec<u8>,
        received: u64,
        over: bool,
    }
    let start = Lines {
        bytes: Box::pin(bytes),
        buffer: Vec::new(),
        received: 0,
        over: false,
    };
    futures::stream::unfold(start, |mut s| async move {
        if s.over {
            return None;
        }
        loop {
            if let Some(end) = s.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = s.buffer.drain(..=end).collect();
                let item = match serde_json::from_slice::<api::TreeLine>(&line[..end]) {
                    Ok(api::TreeLine::Node(node)) => {
                        s.received += 1;
                        Ok(node)
                    }
                    Ok(api::TreeLine::End { count }) if count == s.received => return None,
                    Ok(api::TreeLine::End { count }) => Err(Error::Decode(format!(
                        "the tree announced {count} nodes, {} came",
                        s.received
                    ))),
                    Ok(api::TreeLine::Error(error)) => Err(Error::Api { status: 500, error }),
                    Err(e) => Err(Error::Decode(e.to_string())),
                };
                s.over = item.is_err();
                return Some((item, s));
            }
            match s.bytes.next().await {
                Some(Ok(chunk)) => s.buffer.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    s.over = true;
                    return Some((Err(e.into()), s));
                }
                None => {
                    s.over = true;
                    return Some((
                        Err(Error::Decode("the tree was cut before its end".into())),
                        s,
                    ));
                }
            }
        }
    })
}

/// A node's outcome in a batch: the node, or its error.
fn node_outcome(o: api::NodeOutcome) -> Result<api::NodeInfo, Error> {
    match (o.node, o.error) {
        (Some(node), _) => Ok(node),
        (None, Some(error)) => Err(Error::Api {
            status: o.status.unwrap_or(409),
            error,
        }),
        (None, None) => Err(Error::Decode("an empty node outcome".into())),
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    fn node(path: &str) -> String {
        format!(
            r#"{{"node":{{"id":"{}","path":"{path}","is_file":true,"modified":null,"current_version":null}}}}"#,
            Uuid::nil()
        )
    }

    /// The bytes as they'd come, in arbitrary pieces.
    async fn read(pieces: Vec<String>) -> Vec<Result<String, String>> {
        let bytes = futures::stream::iter(
            pieces
                .into_iter()
                .map(|p| Ok::<_, reqwest::Error>(bytes::Bytes::from(p))),
        );
        tree_lines(bytes)
            .map(|r| r.map(|n| n.path).map_err(|e| e.to_string()))
            .collect()
            .await
    }

    #[tokio::test]
    async fn complete() {
        let whole = format!(
            "{}\n{}\n{{\"end\":{{\"count\":2}}}}\n",
            node("a"),
            node("a/b")
        );
        // Split in the middle of a line: lines are rebuilt across pieces.
        let (one, two) = whole.split_at(30);
        let got = read(vec![one.into(), two.into()]).await;
        assert_eq!(got, [Ok("a".into()), Ok("a/b".into())]);
    }

    #[tokio::test]
    async fn cut_before_the_end() {
        let got = read(vec![format!("{}\n", node("a"))]).await;
        assert_eq!(got[0], Ok("a".into()));
        assert!(got[1].as_ref().unwrap_err().contains("cut before its end"));
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn wrong_count_or_error() {
        let got = read(vec![format!(
            "{}\n{{\"end\":{{\"count\":3}}}}\n",
            node("a")
        )])
        .await;
        assert!(got[1].as_ref().unwrap_err().contains("3 nodes, 1 came"));
        let failed = r#"{"error":{"code":"internal","message":"boom","chunks":[]}}"#;
        let got = read(vec![format!("{failed}\n")]).await;
        assert!(got[0].as_ref().unwrap_err().contains("boom"));
    }
}
