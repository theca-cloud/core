//! Where uploads go, for every client: a file named like one already in the
//! folder (as the server compares names) becomes a new version of it, any
//! other a new file; a tree of files goes under a folder, its folders created
//! like `mkdir -p`. Clients walk their own files (a local folder, a browser's
//! dropped ones) and hand over paths.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use theca_common::name::Name;
use uuid::Uuid;

use crate::api::{Client, Error};
use crate::upload::Target;

/// A node already where a file goes.
#[derive(Debug, Clone, Copy)]
pub struct Existing {
    pub id: Uuid,
    pub is_file: bool,
    pub current_version: Option<Uuid>,
}

/// A `/`-separated path compared like the server's names (a map key).
pub fn path_key(path: &str) -> String {
    path.split('/')
        .map(|name| {
            Name::new(name)
                .map_or_else(|_| name.to_owned(), |n| n.as_str().to_owned())
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A file as `name` in the folder `parent`: a new version of `existing` (based
/// on its current version), or a new file. A folder with that name refuses it.
pub fn target(
    library: Uuid,
    parent: Uuid,
    name: &str,
    existing: Option<&Existing>,
) -> Result<Target, Error> {
    let name = Name::new(name).map_err(|e| Error::InvalidName(format!("{name}: {e}")))?;
    match existing {
        Some(e) if e.is_file => Ok(Target::Version {
            library,
            file: e.id,
            base: e.current_version,
        }),
        Some(_) => Err(Error::FolderExists(name.as_str().to_owned())),
        None => Ok(Target::New {
            library,
            parent,
            name,
        }),
    }
}

/// The most folders `POST /nodes/{id}/folders` takes at once.
const FOLDERS_BATCH: usize = 1000;

/// Files (`/`-separated paths below `folder`: `a.txt`, `a/b/c.txt`) placed in
/// the folder `folder` of `library`: their folders created (`mkdir -p`, with
/// `folders`, empty ones included), what's there already read from one
/// listing per existing top folder, each file's target or its own error (an
/// invalid name: its folder's files fail, the others go on). `Err`: nothing
/// could be placed (the server unreachable, the folder gone).
pub async fn place(
    client: &Client,
    library: Uuid,
    folder: Uuid,
    folders: &[String],
    files: &[String],
) -> Result<Vec<Result<Target, Error>>, Error> {
    // Every folder (the files' too), parents first; folders with invalid
    // names left out, with what's below them.
    let split = |path: &str| -> (String, String) {
        match path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_owned(), name.to_owned()),
            None => (String::new(), path.to_owned()),
        }
    };
    let mut all: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for path in folders
        .iter()
        .cloned()
        .chain(files.iter().map(|f| split(f).0))
    {
        let mut prefix = String::new();
        for name in path.split('/').filter(|n| !n.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(name);
            if seen.insert(path_key(&prefix)) {
                all.push(prefix.clone());
            }
        }
    }
    let invalid: HashSet<String> = all
        .iter()
        .filter(|p| p.split('/').any(|n| Name::new(n).is_err()))
        .map(|p| path_key(p))
        .collect();
    all.retain(|p| !invalid.contains(&path_key(p)));
    all.sort_by_key(|p| p.matches('/').count());

    let mut ids: HashMap<String, Uuid> = HashMap::new();
    let mut existing: HashMap<String, Existing> = HashMap::new();
    for batch in all.chunks(FOLDERS_BATCH) {
        for (path, made) in batch
            .iter()
            .zip(client.create_folders(folder, batch).await?)
        {
            ids.insert(path_key(path), made.id);
            // A top folder that was there: what's in it, from one listing.
            if !made.created && !path.contains('/') {
                let mut nodes = std::pin::pin!(client.tree(made.id).await?);
                while let Some(node) = nodes.next().await {
                    let node = node?;
                    existing.insert(
                        path_key(&format!("{path}/{}", node.path)),
                        Existing {
                            id: node.id,
                            is_file: node.is_file,
                            current_version: node.current_version,
                        },
                    );
                }
            }
        }
    }
    if files.iter().any(|f| !f.contains('/')) {
        for node in client.children(folder).await? {
            existing.insert(
                path_key(&node.name),
                Existing {
                    id: node.id,
                    is_file: node.is_file,
                    current_version: node.current_version,
                },
            );
        }
    }

    Ok(files
        .iter()
        .map(|path| {
            let (dir, name) = split(path);
            let parent = if dir.is_empty() {
                folder
            } else {
                *ids.get(&path_key(&dir))
                    .ok_or_else(|| Error::InvalidName(format!("{dir}: an invalid folder name")))?
            };
            target(library, parent, &name, existing.get(&path_key(path)))
        })
        .collect())
}
