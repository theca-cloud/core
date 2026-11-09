//! WebDAV (`/dav/`), over `dav-server`, with app passwords as Basic auth.
//!
//! The tree follows the app password's scope: the whole account (`/dav/` lists
//! organizations, the personal one as `My files`, then libraries), one
//! organization (`/dav/` lists its libraries), or one library (`/dav/` is its
//! root). Every operation goes through [`crate::files`], like the API; reads
//! stream through [`crate::stream`].

use std::collections::HashMap;
use std::io::{SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::{Buf, Bytes};
use chrono::{DateTime, Utc};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavMetaData, DavProp, FsError, FsFuture, FsResult, FsStream,
    GuardedFileSystem, OpenOptions, ReadDirMeta,
};
use dav_server::{DavHandler, memls::MemLs};
use futures::FutureExt;
use theca_common::ignore::{self as ignore_file, Ignores};
use theca_common::name::{Name, is_ignored};
use uuid::Uuid;

use crate::actions::node::{self as files, Target};
use crate::actions::{self, FileError};
use crate::api::AppState;
use crate::db;
use crate::db::account::Credential;
use crate::db::library::NodeAccess;
use crate::stream::VersionReader;

/// The WebDAV handler, built once: our filesystem, and locks in memory (one
/// process). The credentials come with each request.
pub fn handler(state: AppState) -> DavHandler<Who> {
    DavHandler::builder()
        .strip_prefix("/dav")
        .filesystem(Box::new(Fs {
            state,
            ignore_files: Arc::default(),
        }))
        .locksystem(MemLs::new())
        .build_handler()
}

/// Serves a WebDAV request. Only app passwords (Basic auth, read by the
/// authentication middleware); never sessions.
pub async fn serve(dav: DavHandler<Who>, request: Request) -> Response {
    let Some(who) = request
        .extensions()
        .get::<Credential>()
        .copied()
        .filter(|c| !c.session)
    else {
        let mut response =
            (StatusCode::UNAUTHORIZED, "an app password is required").into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Theca\", charset=\"UTF-8\""),
        );
        return response;
    };
    // The file's modification time on the client, as ownCloud and Nextcloud
    // take it (rclone's `vendor = owncloud` sends it): Unix seconds.
    let modified = match request.headers().get("x-oc-mtime") {
        None => None,
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .and_then(|secs| DateTime::from_timestamp(secs as i64, 0))
            .filter(|t| db::file::plausible_modified(*t))
        {
            Some(t) => Some(t),
            None => return (StatusCode::BAD_REQUEST, "invalid X-OC-Mtime").into_response(),
        },
    };
    let put = request.method() == Method::PUT;
    let who = Who {
        credential: who,
        modified,
        trashed: Arc::default(),
    };
    let mut response = dav
        .handle_guarded(request, who.account_uuid.to_string(), who)
        .await
        .map(Body::new);
    if put && modified.is_some() && response.status().is_success() {
        response
            .headers_mut()
            .insert("x-oc-mtime", HeaderValue::from_static("accepted"));
    }
    response
}

/// Who a request acts as, and what it trashed so far: `dav-server` deletes a
/// folder bottom-up (each file, then the folder), and the API's trash flags
/// only the folder. So trashing a folder un-trashes the children this same
/// request trashed just before: one trash entry, restorable as a whole.
#[derive(Clone)]
pub struct Who {
    credential: Credential,
    /// A `PUT`'s `X-OC-Mtime`: the file's modification time on the client.
    modified: Option<DateTime<Utc>>,
    trashed: Arc<Mutex<Vec<Trashed>>>,
}

/// A node this request trashed: `(node, its parent)`.
type Trashed = (i64, Option<i64>);

impl std::ops::Deref for Who {
    type Target = Credential;

    fn deref(&self) -> &Credential {
        &self.credential
    }
}

/// The filesystem: nodes, with virtual levels above the libraries.
#[derive(Clone)]
pub struct Fs {
    state: AppState,
    /// Ignore files' content (their first `ignore::MAX_LEN` bytes and one
    /// more) by version: a version never changes, so a copy of many files
    /// reads each ignore file once.
    ignore_files: Arc<Mutex<HashMap<Uuid, Arc<[u8]>>>>,
}

/// The most ignore files' contents kept (each at most 64 KiB): past it, the
/// cache starts over.
const IGNORE_FILES_CACHED: usize = 1024;

/// What a path names.
enum Item {
    /// `/dav/` for an account- or organization-scoped password.
    Root,
    /// An organization (account scope): lists its libraries.
    Organization { pk: i64 },
    /// A library's root, or anything below.
    Node(NodeAccess),
}

fn map(e: FileError) -> FsError {
    match e {
        FileError::NotFound(_) => FsError::NotFound,
        FileError::NameTaken => FsError::Exists,
        FileError::QuotaExceeded => FsError::InsufficientStorage,
        FileError::ReadOnly
        | FileError::Root
        | FileError::OtherOrganization
        | FileError::MoveIntoItself
        | FileError::InvalidName => FsError::Forbidden,
        other => {
            tracing::error!(error = %other, "WebDAV operation failed");
            FsError::GeneralFailure
        }
    }
}

fn db_error(e: sqlx::Error) -> FsError {
    tracing::error!(error = %e, "WebDAV database error");
    FsError::GeneralFailure
}

/// A path's segments, normalized like stored names (NFC).
fn segments(path: &DavPath) -> FsResult<Vec<String>> {
    path.as_rel_ospath()
        .components()
        .map(|c| {
            let raw = c.as_os_str().to_str().ok_or(FsError::NotFound)?;
            Ok(Name::new(raw).map_or_else(|_| raw.to_owned(), |n| n.as_str().to_owned()))
        })
        .collect()
}

impl Fs {
    /// The organizations an account-scoped password shows, with their display
    /// names (`My files` for the personal one; ` (2)` for duplicates).
    async fn organizations(&self, who: &Credential) -> FsResult<Vec<(i64, String)>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT o.pk, o.name, o.personal_of IS NOT NULL AS "personal!"
               FROM organization_access($1) a JOIN organization o ON o.pk = a.organization
               ORDER BY o.pk"#,
            who.token
        )
        .fetch_all(&self.state.pool)
        .await
        .map_err(db_error)?;
        let mut out: Vec<(i64, String)> = Vec::new();
        for r in rows {
            let base = if r.personal {
                "My files".to_owned()
            } else {
                r.name
            };
            let mut name = base.clone();
            let mut n = 2;
            while out
                .iter()
                .any(|(_, taken)| taken.to_lowercase() == name.to_lowercase())
            {
                name = format!("{base} ({n})");
                n += 1;
            }
            out.push((r.pk, name));
        }
        Ok(out)
    }

    /// The libraries of an organization the password reaches: `(name, root)`.
    async fn libraries(
        &self,
        who: &Credential,
        organization: i64,
    ) -> FsResult<Vec<(String, Uuid)>> {
        let rows = sqlx::query!(
            "SELECT l.name, n.uuid AS root FROM library_access($1) a
             JOIN library l ON l.pk = a.library
             JOIN node n ON n.library = l.pk AND n.parent IS NULL
             WHERE l.owner = $2 ORDER BY lower(l.name)",
            who.token,
            organization
        )
        .fetch_all(&self.state.pool)
        .await
        .map_err(db_error)?;
        Ok(rows.into_iter().map(|r| (r.name, r.root)).collect())
    }

    async fn node(&self, who: &Credential, uuid: Uuid) -> FsResult<NodeAccess> {
        db::library::node_access(&self.state.pool, who, uuid)
            .await
            .map_err(db_error)?
            .ok_or(FsError::NotFound)
    }

    /// A reachable child by name (case-insensitive, like uniqueness).
    async fn child(
        &self,
        who: &Credential,
        parent: &NodeAccess,
        name: &str,
    ) -> FsResult<Option<NodeAccess>> {
        let uuid = sqlx::query_scalar!(
            "SELECT uuid FROM node WHERE parent = $1 AND lower(name) = lower($2) AND deleted_at IS NULL",
            parent.node,
            name
        )
        .fetch_optional(&self.state.pool)
        .await
        .map_err(db_error)?;
        match uuid {
            Some(uuid) => Ok(Some(self.node(who, uuid).await?)),
            None => Ok(None),
        }
    }

    /// Resolves a path, following the password's scope.
    async fn resolve(&self, who: &Credential, segments: &[String]) -> FsResult<Item> {
        match self.deepest(who, segments).await? {
            (item, found) if found == segments.len() => Ok(item),
            _ => Err(FsError::NotFound),
        }
    }

    /// The deepest item of a path that exists, and how many of its segments
    /// that took.
    async fn deepest(&self, who: &Credential, segments: &[String]) -> FsResult<(Item, usize)> {
        let mut rest = segments.iter();
        let mut item = match (who.library, who.organization) {
            (Some(library), _) => {
                let root = sqlx::query_scalar!(
                    "SELECT uuid FROM node WHERE library = $1 AND parent IS NULL",
                    library
                )
                .fetch_one(&self.state.pool)
                .await
                .map_err(db_error)?;
                Item::Node(self.node(who, root).await?)
            }
            (None, Some(organization)) => Item::Organization { pk: organization },
            (None, None) => Item::Root,
        };
        let mut found = 0;
        for segment in rest.by_ref() {
            let next = match &item {
                Item::Root => {
                    let orgs = self.organizations(who).await?;
                    orgs.into_iter()
                        .find(|(_, name)| name.to_lowercase() == segment.to_lowercase())
                        .map(|(pk, _)| Item::Organization { pk })
                }
                Item::Organization { pk } => {
                    match self
                        .libraries(who, *pk)
                        .await?
                        .into_iter()
                        .find(|(name, _)| name.to_lowercase() == segment.to_lowercase())
                    {
                        Some((_, root)) => Some(Item::Node(self.node(who, root).await?)),
                        None => None,
                    }
                }
                Item::Node(node) if !node.is_file => {
                    self.child(who, node, segment).await?.map(Item::Node)
                }
                Item::Node(_) => None,
            };
            let Some(next) = next else { break };
            item = next;
            found += 1;
        }
        Ok((item, found))
    }

    /// Whether `below` (names under `folder`, the target's last) is an
    /// ignored file or folder: named by an ignore file in `folder` or above
    /// it. What WebDAV writes there is discarded.
    async fn ignored(&self, folder: &NodeAccess, below: &[String], is_dir: bool) -> FsResult<bool> {
        let found = db::node::ignore_files(&self.state.pool, folder.node)
            .await
            .map_err(db_error)?;
        let mut ignores = Ignores::new();
        for file in &found.files {
            let Some(version) = &file.version else {
                continue;
            };
            let (content, fresh) = self.ignore_file(version.pk, version.id).await?;
            let problems = ignores.add(&found.folder[..file.depth], &content);
            if fresh {
                for problem in problems {
                    let path = found.path(file);
                    tracing::warn!(version = %version.id, path, %problem,
                        "part of an ignore file doesn't apply");
                }
            }
        }
        let mut path = found.folder;
        path.extend_from_slice(below);
        Ok(ignores.is_ignored(&path, is_dir))
    }

    /// An ignore file's content, as much as applies (and a byte more, which
    /// tells it was longer); `true` when just read from storage.
    async fn ignore_file(&self, pk: i64, version: Uuid) -> FsResult<(Arc<[u8]>, bool)> {
        if let Some(content) = self
            .ignore_files
            .lock()
            .expect("not poisoned")
            .get(&version)
        {
            return Ok((content.clone(), false));
        }
        let failed = |e| {
            tracing::error!(error = %e, "WebDAV: reading an ignore file");
            FsError::GeneralFailure
        };
        let mut reader = VersionReader::open(&self.state, pk).await.map_err(failed)?;
        let want = (reader.len() as usize).min(ignore_file::MAX_LEN + 1);
        let mut content = Vec::with_capacity(want);
        while content.len() < want {
            let bytes = reader
                .read(content.len() as u64, want - content.len())
                .await
                .map_err(failed)?;
            if bytes.is_empty() {
                break;
            }
            content.extend_from_slice(&bytes);
        }
        let content: Arc<[u8]> = content.into();
        let mut cache = self.ignore_files.lock().expect("not poisoned");
        if cache.len() >= IGNORE_FILES_CACHED {
            cache.clear();
        }
        cache.insert(version, content.clone());
        Ok((content, true))
    }

    /// Where a write goes: the folder a new item goes into, and its name; none
    /// when an ignore file names the path (`is_dir`: a folder), which WebDAV
    /// answers without storing anything. Decided before the parent must
    /// exist: an ignored folder was never created. A folder already there is
    /// answered as one, ignored or not.
    async fn write_target(
        &self,
        who: &Credential,
        path: &DavPath,
        is_dir: bool,
    ) -> FsResult<Option<(Item, String)>> {
        let mut segments = segments(path)?;
        let name = segments.pop().ok_or(FsError::Forbidden)?;
        let (parent, found) = self.deepest(who, &segments).await?;
        if let Item::Node(folder) = &parent
            && !folder.is_file
        {
            if is_dir && found == segments.len() && self.child(who, folder, &name).await?.is_some()
            {
                return Err(FsError::Exists);
            }
            let mut below = segments[found..].to_vec();
            below.push(name.clone());
            if self.ignored(folder, &below, is_dir).await? {
                return Ok(None);
            }
        }
        if found < segments.len() {
            return Err(FsError::NotFound);
        }
        Ok(Some((parent, name)))
    }

    /// The folder a new item goes into, and its name.
    async fn parent_of(&self, who: &Credential, path: &DavPath) -> FsResult<(Item, String)> {
        let mut segments = segments(path)?;
        let name = segments.pop().ok_or(FsError::Forbidden)?;
        Ok((self.resolve(who, &segments).await?, name))
    }

    async fn metadata_of(&self, item: &Item) -> FsResult<Meta> {
        match item {
            Item::Root | Item::Organization { .. } => Ok(Meta::dir(UNIX_EPOCH)),
            Item::Node(node) => {
                let row = sqlx::query!(
                    r#"SELECT n.is_file, v.uuid AS "version?", extract(epoch FROM v.modified_at)::float8 AS "modified?",
                              v.size AS "len?"
                       FROM node n LEFT JOIN version v ON v.pk = n.current_version WHERE n.pk = $1"#,
                    node.node
                )
                .fetch_one(&self.state.pool)
                .await
                .map_err(db_error)?;
                let modified = time(row.modified);
                Ok(if row.is_file {
                    Meta::file(row.len.unwrap_or(0) as u64, modified, row.version)
                } else {
                    Meta::dir(modified)
                })
            }
        }
    }
}

fn time(epoch: Option<f64>) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(epoch.unwrap_or(0.0).max(0.0))
}

impl GuardedFileSystem<Who> for Fs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
        who: &'a Who,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            if !options.write {
                let Item::Node(node) = self.resolve(who, &segments(path)?).await? else {
                    return Err(FsError::Forbidden);
                };
                let version = node
                    .current_version
                    .filter(|_| node.is_file)
                    .ok_or(FsError::NotFound)?;
                let meta = self.metadata_of(&Item::Node(node)).await?;
                let reader = VersionReader::open(&self.state, version)
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "WebDAV read");
                        FsError::GeneralFailure
                    })?;
                return Ok(Box::new(ReadFile {
                    reader,
                    position: 0,
                    meta,
                }) as Box<dyn DavFile>);
            }
            let Some((parent, name)) = self.write_target(who, path, false).await? else {
                return Ok(Box::new(Discarded::default()) as Box<dyn DavFile>);
            };
            let Item::Node(parent) = parent else {
                return Err(FsError::Forbidden);
            };
            let existing = self.child(who, &parent, &name).await?;
            if existing.is_some() && options.create_new {
                return Err(FsError::Exists);
            }
            if existing.as_ref().is_some_and(|e| !e.is_file) {
                return Err(FsError::Forbidden);
            }
            if existing.is_none() && !options.create {
                return Err(FsError::NotFound);
            }
            let spool = tempfile::NamedTempFile::new().map_err(|_| FsError::InsufficientStorage)?;
            Ok(Box::new(WriteFile {
                fs: self.clone(),
                who: who.credential,
                modified: who.modified,
                parent,
                existing,
                name,
                spool,
                len: 0,
                stored: false,
            }) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
        who: &'a Who,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let entries: Vec<Entry> = match self.resolve(who, &segments(path)?).await? {
                Item::Root => self
                    .organizations(who)
                    .await?
                    .into_iter()
                    .map(|(_, name)| Entry {
                        name,
                        meta: Meta::dir(UNIX_EPOCH),
                    })
                    .collect(),
                Item::Organization { pk } => self
                    .libraries(who, pk)
                    .await?
                    .into_iter()
                    .map(|(name, _)| Entry {
                        name,
                        meta: Meta::dir(UNIX_EPOCH),
                    })
                    .collect(),
                Item::Node(node) if !node.is_file => {
                    let rows = sqlx::query!(
                        r#"SELECT n.name, n.is_file, v.uuid AS "version?",
                                  extract(epoch FROM v.modified_at)::float8 AS "modified?",
                                  v.size AS "len?"
                           FROM node n LEFT JOIN version v ON v.pk = n.current_version
                           WHERE n.parent = $1 AND n.deleted_at IS NULL ORDER BY lower(n.name)"#,
                        node.node
                    )
                    .fetch_all(&self.state.pool)
                    .await
                    .map_err(db_error)?;
                    rows.into_iter()
                        .map(|r| Entry {
                            name: r.name,
                            meta: if r.is_file {
                                Meta::file(r.len.unwrap_or(0) as u64, time(r.modified), r.version)
                            } else {
                                Meta::dir(time(r.modified))
                            },
                        })
                        .collect()
                }
                Item::Node(_) => return Err(FsError::Forbidden),
            };
            let stream = futures::stream::iter(
                entries
                    .into_iter()
                    .map(|e| Ok(Box::new(e) as Box<dyn DavDirEntry>)),
            );
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn metadata<'a>(
        &'a self,
        path: &'a DavPath,
        who: &'a Who,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let item = self.resolve(who, &segments(path)?).await?;
            Ok(Box::new(self.metadata_of(&item).await?) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        async move {
            // An ignored folder: answered, not created, and every write below
            // it discarded the same way.
            let Some((parent, name)) = self.write_target(who, path, true).await? else {
                return Ok(());
            };
            let name = Name::new(&name).map_err(|_| FsError::Forbidden)?;
            match parent {
                Item::Node(parent) => {
                    files::create_folders(&self.state, &parent, &[name.as_str().to_owned()])
                        .await
                        .map_err(map)?;
                    Ok(())
                }
                // A folder in an organization is a new library.
                Item::Organization { pk } => self.create_library(who, pk, name.as_str()).await,
                Item::Root => Err(FsError::Forbidden),
            }
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        self.remove(path, who)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        self.remove(path, who)
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        async move {
            let Item::Node(node) = self.resolve(who, &segments(from)?).await? else {
                return Err(FsError::Forbidden);
            };
            let (parent, name) = self.parent_of(who, to).await?;
            let Item::Node(parent) = parent else {
                return Err(FsError::Forbidden);
            };
            let name = Name::new(&name).map_err(|_| FsError::Forbidden)?;
            files::rename_move(
                &self.state,
                &node,
                Some(name.as_str()),
                Some(&parent),
                false,
            )
            .await
            .map(|_| ())
            .map_err(map)
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        async move {
            let Item::Node(node) = self.resolve(who, &segments(from)?).await? else {
                return Err(FsError::Forbidden);
            };
            let (parent, name) = self.parent_of(who, to).await?;
            let Item::Node(parent) = parent else {
                return Err(FsError::Forbidden);
            };
            let name = Name::new(&name).map_err(|_| FsError::Forbidden)?;
            files::copy(&self.state, &node, &parent, Some(name.as_str()), false)
                .await
                .map_err(map)?;
            Ok(())
        }
        .boxed()
    }

    // Dead properties are accepted (clients set them, e.g. Windows timestamps)
    // but not stored: per-node properties are a later feature (features.md).
    fn have_props<'a>(
        &'a self,
        _path: &'a DavPath,
        _who: &'a Who,
    ) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(true))
    }

    fn patch_props<'a>(
        &'a self,
        _path: &'a DavPath,
        patch: Vec<(bool, DavProp)>,
        _who: &'a Who,
    ) -> FsFuture<'a, Vec<(StatusCode, DavProp)>> {
        let ok = patch
            .into_iter()
            .map(|(_, p)| (StatusCode::OK, p))
            .collect();
        Box::pin(std::future::ready(Ok(ok)))
    }

    fn get_props<'a>(
        &'a self,
        _path: &'a DavPath,
        _content: bool,
        _who: &'a Who,
    ) -> FsFuture<'a, Vec<DavProp>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    fn get_prop<'a>(
        &'a self,
        _path: &'a DavPath,
        _prop: DavProp,
        _who: &'a Who,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(std::future::ready(Err(FsError::NotFound)))
    }
}

impl Fs {
    fn remove<'a>(&'a self, path: &'a DavPath, who: &'a Who) -> FsFuture<'a, ()> {
        async move {
            let Item::Node(node) = self.resolve(who, &segments(path)?).await? else {
                return Err(FsError::Forbidden);
            };
            files::trash(&self.state, &node).await.map_err(map)?;
            let children: Vec<i64> = {
                let mut trashed = who.trashed.lock().expect("not poisoned");
                trashed.push((node.node, node.parent));
                trashed
                    .iter()
                    .filter(|(_, parent)| *parent == Some(node.node))
                    .map(|(pk, _)| *pk)
                    .collect()
            };
            if !children.is_empty() {
                files::untrash_below_trashed(&self.state, &children)
                    .await
                    .map_err(map)?;
            }
            Ok(())
        }
        .boxed()
    }

    async fn create_library(
        &self,
        who: &Credential,
        organization: i64,
        name: &str,
    ) -> FsResult<()> {
        let uuid = sqlx::query_scalar!("SELECT uuid FROM organization WHERE pk = $1", organization)
            .fetch_one(&self.state.pool)
            .await
            .map_err(db_error)?;
        actions::library::create(&self.state, who, uuid, name, &[])
            .await
            .map_err(|e| match e {
                db::library::CreateError::NotMember => FsError::Forbidden,
                db::library::CreateError::Db(e)
                    if e.as_database_error().and_then(|d| d.constraint())
                        == Some("library_name") =>
                {
                    FsError::Exists
                }
                _ => FsError::GeneralFailure,
            })?;
        Ok(())
    }
}

/// Metadata: size, modification time, type, ETag (the version).
#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    modified: SystemTime,
    dir: bool,
    version: Option<Uuid>,
}

impl Meta {
    fn dir(modified: SystemTime) -> Self {
        Self {
            len: 0,
            modified,
            dir: true,
            version: None,
        }
    }

    fn file(len: u64, modified: SystemTime, version: Option<Uuid>) -> Self {
        Self {
            len,
            modified,
            dir: false,
            version,
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn is_dir(&self) -> bool {
        self.dir
    }

    fn etag(&self) -> Option<String> {
        self.version.map(|v| v.to_string())
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }
}

/// A file being read: the streaming reader and a position.
struct ReadFile {
    reader: VersionReader,
    position: u64,
    meta: Meta,
}

impl std::fmt::Debug for ReadFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadFile")
            .field("position", &self.position)
            .finish()
    }
}

impl DavFile for ReadFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }

    fn write_buf(&mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let bytes = self.reader.read(self.position, count).await.map_err(|e| {
                tracing::error!(error = %e, "WebDAV read");
                FsError::GeneralFailure
            })?;
            self.position += bytes.len() as u64;
            Ok(bytes)
        }
        .boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let len = self.reader.len() as i64;
        let target = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => len + d,
            SeekFrom::Current(d) => self.position as i64 + d,
        };
        if target < 0 {
            return Box::pin(std::future::ready(Err(FsError::GeneralFailure)));
        }
        self.position = target as u64;
        Box::pin(std::future::ready(Ok(self.position)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A write to an ignored path: its bytes counted, never stored.
#[derive(Debug, Default)]
struct Discarded {
    len: u64,
}

impl DavFile for Discarded {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = Meta::file(self.len, SystemTime::now(), None);
        Box::pin(std::future::ready(Ok(
            Box::new(meta) as Box<dyn DavMetaData>
        )))
    }

    fn write_buf(&mut self, buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        self.len += buf.remaining() as u64;
        Box::pin(std::future::ready(Ok(())))
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        self.len += buf.len() as u64;
        Box::pin(std::future::ready(Ok(())))
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(std::future::ready(Err(FsError::NotImplemented)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A file being written: spooled to a temporary file, stored (chunked,
/// deduplicated, committed) on flush.
struct WriteFile {
    fs: Fs,
    who: Credential,
    /// The request's `X-OC-Mtime`.
    modified: Option<DateTime<Utc>>,
    parent: NodeAccess,
    existing: Option<NodeAccess>,
    name: String,
    spool: tempfile::NamedTempFile,
    len: u64,
    stored: bool,
}

impl std::fmt::Debug for WriteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteFile")
            .field("name", &self.name)
            .field("len", &self.len)
            .finish()
    }
}

impl DavFile for WriteFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = Meta::file(self.len, SystemTime::now(), None);
        Box::pin(std::future::ready(Ok(
            Box::new(meta) as Box<dyn DavMetaData>
        )))
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let bytes = buf.copy_to_bytes(buf.remaining());
        self.write_bytes(bytes)
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        // ponytail: a blocking write to a local temp file; fine for upload speeds.
        let result = self
            .spool
            .write_all(&buf)
            .map(|()| self.len += buf.len() as u64)
            .map_err(|_| FsError::InsufficientStorage);
        Box::pin(std::future::ready(result))
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(std::future::ready(Err(FsError::NotImplemented)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move {
            // What the operating system leaves (`._*`, `.DS_Store`...):
            // discarded, as written (a refusal can abort Finder's whole copy).
            if self.stored || is_ignored(&self.name) {
                return Ok(());
            }
            self.spool
                .flush()
                .map_err(|_| FsError::InsufficientStorage)?;
            let name = Name::new(&self.name).map_err(|_| FsError::Forbidden)?;
            let target = match &self.existing {
                Some(file) => Target::Existing(file),
                None => Target::New {
                    parent: &self.parent,
                    name: name.as_str(),
                },
            };
            files::store(
                &self.fs.state,
                &self.who,
                target,
                self.spool.path(),
                self.modified,
            )
            .await
            .map_err(map)?;
            self.stored = true;
            Ok(())
        }
        .boxed()
    }
}
