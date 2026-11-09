//! Where uploads go, for every client: a file named like one already in the
//! folder (as the server compares names) becomes a new version of it, any
//! other a new file; a tree of files goes under a folder, its folders created
//! like `mkdir -p`. Clients walk their own files (a local folder, a browser's
//! dropped ones) and hand over paths. What an ignore file names is skipped:
//! never uploaded, its folders never created.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use theca_common::ignore::{self as ignore_file, Ignores, Problem};
use theca_common::name::{Name, is_ignored};
use uuid::Uuid;

use crate::api::{Client, Error};
use crate::download::{DownloadOptions, stream};
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
/// on its current version), or a new file. A folder with that name refuses it;
/// a file the operating system left is skipped (`Error::Ignored`), even if one
/// is already there.
pub fn target(
    library: Uuid,
    parent: Uuid,
    name: &str,
    existing: Option<&Existing>,
) -> Result<Target, Error> {
    if is_ignored(name) {
        return Err(Error::Ignored(name.to_owned()));
    }
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

/// What [`place`] decided: each file's target or its own error, in order,
/// and what in the ignore files that applied doesn't apply (by the ignore
/// file's path below `folder`, or from the library's root when above it).
#[derive(Debug)]
pub struct Placed {
    pub targets: Vec<Result<Target, Error>>,
    pub problems: Vec<(String, Problem)>,
}

/// Files (`/`-separated paths below `folder`: `a.txt`, `a/b/c.txt`) placed in
/// the folder `folder` of `library`: their folders created (`mkdir -p`, with
/// `folders`, empty ones included), what's there already read from one
/// listing per existing top folder, each file's target or its own error (an
/// invalid name: its folder's files fail, the others go on).
///
/// What an ignore file names is skipped (`Error::IgnoreFile`), its folders
/// not created: the ignore files of `folder` and above (from the server),
/// those already below it (from the listings), and the dropped ones,
/// `ignore_files` (paths among `files`, with their content), each in place of
/// the server's in the same folder. `Err`: nothing could be placed (the server
/// unreachable, the folder gone).
pub async fn place(
    client: &Client,
    library: Uuid,
    folder: Uuid,
    folders: &[String],
    files: &[String],
    ignore_files: &[(String, Vec<u8>)],
) -> Result<Placed, Error> {
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

    // What's there already: the folder's children, and the tree of each top
    // folder that exists (ignore files below included).
    let mut existing: HashMap<String, Existing> = HashMap::new();
    let mut below: Vec<(String, Uuid, Uuid)> = Vec::new();
    let tops: HashSet<String> = all
        .iter()
        .filter(|p| !p.contains('/'))
        .map(|p| path_key(p))
        .collect();
    for node in client.children(folder).await? {
        let key = path_key(&node.name);
        if !node.is_file && tops.contains(&key) {
            let mut nodes = std::pin::pin!(client.tree(node.id).await?);
            while let Some(child) = nodes.next().await {
                let child = child?;
                let path = format!("{}/{}", node.name, child.path);
                if child.is_file
                    && ignore_file::is_ignore_file(child.path.rsplit('/').next().unwrap_or(""))
                    && let Some(version) = child.current_version
                {
                    below.push((path.clone(), child.id, version));
                }
                existing.insert(
                    path_key(&path),
                    Existing {
                        id: child.id,
                        is_file: child.is_file,
                        current_version: child.current_version,
                    },
                );
            }
        }
        existing.insert(
            key,
            Existing {
                id: node.id,
                is_file: node.is_file,
                current_version: node.current_version,
            },
        );
    }

    // The ignore files that apply, by their folder from the library's root.
    let above = client.ignore_files(folder).await?;
    let root: Vec<String> = names(&above.path);
    let mut ignores = Ignores::new();
    let mut problems = Vec::new();
    for file in &above.files {
        let Some(version) = file.current_version else {
            continue;
        };
        let content = read(client, file.id, version).await?;
        let mut its_folder = names(&file.path);
        its_folder.pop();
        for problem in ignores.add(&its_folder, &content) {
            problems.push((file.path.clone(), problem));
        }
    }
    let dropped: HashSet<String> = ignore_files
        .iter()
        .map(|(path, _)| path_key(&split(path).0))
        .collect();
    let mut add = |path: &str, content: &[u8], problems: &mut Vec<(String, Problem)>| {
        let mut folder = root.clone();
        folder.extend(names(&split(path).0));
        for problem in ignores.add(&folder, content) {
            problems.push((path.to_owned(), problem));
        }
    };
    // Below: only those of the folders the upload goes into (another
    // folder's never applies, nor are its problems said).
    let touched: HashSet<String> = all.iter().map(|p| path_key(p)).collect();
    for (path, id, version) in &below {
        let its_folder = path_key(&split(path).0);
        if touched.contains(&its_folder) && !dropped.contains(&its_folder) {
            add(path, &read(client, *id, *version).await?, &mut problems);
        }
    }
    for (path, content) in ignore_files {
        add(path, content, &mut problems);
    }
    let ignored = |path: &str, is_dir: bool| {
        let mut full = root.clone();
        full.extend(names(path));
        ignores.is_ignored(&full, is_dir)
    };

    // The folders not ignored, created.
    all.retain(|p| !ignored(p, true));
    let mut ids: HashMap<String, Uuid> = HashMap::new();
    for batch in all.chunks(FOLDERS_BATCH) {
        for (path, made) in batch
            .iter()
            .zip(client.create_folders(folder, batch).await?)
        {
            ids.insert(path_key(path), made.id);
        }
    }

    let targets = files
        .iter()
        .map(|path| {
            let (dir, name) = split(path);
            // What the operating system left stays silent, even in an
            // ignored folder (never created: checked before its parent).
            if is_ignored(&name) {
                return Err(Error::Ignored(name));
            }
            if ignored(path, false) {
                return Err(Error::IgnoreFile(path.clone()));
            }
            let parent = if dir.is_empty() {
                folder
            } else {
                *ids.get(&path_key(&dir))
                    .ok_or_else(|| Error::InvalidName(format!("{dir}: an invalid folder name")))?
            };
            target(library, parent, &name, existing.get(&path_key(path)))
        })
        .collect();
    Ok(Placed { targets, problems })
}

/// A path's names (`/a/b`, `a/b`: `a`, `b`; `/`: none).
fn names(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect()
}

/// An ignore file's content, as much as applies (and a byte more, which tells
/// it was longer): read chunk by chunk, the rest never fetched.
async fn read(client: &Client, file: Uuid, version: Uuid) -> Result<Vec<u8>, Error> {
    let mut content = Vec::new();
    let mut chunks = std::pin::pin!(stream(
        client.clone(),
        file,
        Some(version),
        DownloadOptions {
            parallel: 1,
            ..DownloadOptions::default()
        },
    ));
    while content.len() <= ignore_file::MAX_LEN {
        match chunks.next().await {
            Some(chunk) => content.extend_from_slice(&chunk?),
            None => break,
        }
    }
    content.truncate(ignore_file::MAX_LEN + 1);
    Ok(content)
}
