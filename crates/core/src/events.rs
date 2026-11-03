//! The event feed (docs/decisions.md, Sync; ADR 0003): reading a credential's
//! events from its positions, collapsed and resolved to the current state, and
//! the registry of open streams that [`crate::actions::commit`] wakes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::sse::Event as SseEvent;
use theca_common::api;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::api::AppState;
use crate::db::account::Credential;

/// Most streams one credential holds open: a long-lived connection escapes
/// the per-request rate limits. A sync client uses one.
pub const STREAMS_PER_CREDENTIAL: usize = 10;

/// How often a stream pings its client (a comment: proxies close silent
/// connections, and a write to a client gone fails), and how often the open
/// streams' credentials are checked.
pub const INTERVAL: Duration = Duration::from_secs(30);

/// Most events one `changes` message covers (before collapsing).
const BATCH: i64 = 1000;

/// The open streams, in the server process (one process per database:
/// decisions.md, Architecture).
pub struct Registry {
    streams: Mutex<HashMap<u64, Entry>>,
    next: AtomicU64,
    /// How often a stream pings its client, and the revocation check runs.
    pub interval: Duration,
}

struct Entry {
    token: i64,
    account: i64,
    libraries: HashSet<i64>,
    signal: Arc<Signal>,
}

/// How a stream is woken.
#[derive(Default)]
pub struct Signal {
    notify: Notify,
    /// Its account's access changed: resend `libraries`.
    access: AtomicBool,
    /// Its credential fell: end.
    closed: AtomicBool,
}

/// A stream's place in the registry, removed when dropped (however the
/// stream ends).
pub struct Registration {
    registry: Arc<Registry>,
    id: u64,
    signal: Arc<Signal>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.id);
    }
}

impl Registry {
    pub fn new(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            streams: Mutex::default(),
            next: AtomicU64::new(0),
            interval,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        self.streams.lock().expect("not poisoned")
    }

    /// A new stream, before its first read (so an event committed meanwhile
    /// still wakes it). `None`: the credential holds too many.
    pub fn register(self: &Arc<Self>, token: i64, account: i64) -> Option<Registration> {
        let mut streams = self.lock();
        if streams.values().filter(|e| e.token == token).count() >= STREAMS_PER_CREDENTIAL {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let signal = Arc::new(Signal::default());
        streams.insert(
            id,
            Entry {
                token,
                account,
                libraries: HashSet::new(),
                signal: signal.clone(),
            },
        );
        Some(Registration {
            registry: self.clone(),
            id,
            signal,
        })
    }

    fn set_libraries(&self, id: u64, libraries: HashSet<i64>) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.libraries = libraries;
        }
    }

    /// Wakes the streams following one of `libraries`, and those of `accounts`
    /// (their access changed).
    // ponytail: scans every stream (tens of microseconds for 10,000); an index
    // by library if a server ever holds tens of thousands.
    pub fn wake(&self, libraries: &[i64], accounts: &[i64]) {
        if libraries.is_empty() && accounts.is_empty() {
            return;
        }
        for entry in self.lock().values() {
            let access = accounts.contains(&entry.account);
            if access {
                entry.signal.access.store(true, Ordering::Relaxed);
            }
            if access || libraries.iter().any(|l| entry.libraries.contains(l)) {
                entry.signal.notify.notify_one();
            }
        }
    }

    /// Ends the streams of credentials that fell.
    fn close(&self, valid: &HashSet<i64>) {
        for entry in self.lock().values() {
            if !valid.contains(&entry.token) {
                entry.signal.closed.store(true, Ordering::Relaxed);
                entry.signal.notify.notify_one();
            }
        }
    }

    fn tokens(&self) -> Vec<i64> {
        let mut tokens: Vec<i64> = self.lock().values().map(|e| e.token).collect();
        tokens.sort_unstable();
        tokens.dedup();
        tokens
    }
}

/// Every `interval`, checks every open stream's credential in one query and
/// ends the streams whose credential was revoked, expired or suspended. Runs
/// for the server's life.
pub async fn watch_revocations(state: AppState) {
    let mut tick = tokio::time::interval(state.events.interval);
    loop {
        tick.tick().await;
        let tokens = state.events.tokens();
        if tokens.is_empty() {
            continue;
        }
        let lifetime = state.settings.get().session_lifetime();
        match crate::db::account::valid_tokens(&state.pool, &tokens, lifetime).await {
            Ok(valid) => state.events.close(&valid.into_iter().collect()),
            Err(e) => tracing::warn!(error = %e, "checking the event streams' credentials"),
        }
    }
}

/// A batch read, and whether it was full (more may follow at once).
struct Batch {
    changes: api::Changes,
    full: bool,
}

/// A stream's state: where it is in each library it follows (by `pk`).
struct Feed {
    token: i64,
    positions: HashMap<i64, (Uuid, i64)>,
}

impl Feed {
    /// The libraries the credential can access now, trash included, each with
    /// its position: kept for those already followed, the client's for those
    /// in `after` (unless beyond the library's last event: `reset`), the last
    /// event's for the others. Libraries out of reach are dropped.
    async fn libraries(
        &mut self,
        state: &AppState,
        after: &BTreeMap<Uuid, i64>,
    ) -> sqlx::Result<Vec<api::FeedLibrary>> {
        let rows = sqlx::query!(
            r#"SELECT l.pk, l.uuid AS id, o.uuid AS organization, l.name, n.uuid AS root,
                      a.can_write AS "writable!", l.deleted_at IS NOT NULL AS "trashed!", l.event_seq
               FROM library_access_any($1) a
               JOIN library l ON l.pk = a.library
               JOIN organization o ON o.pk = l.owner
               JOIN node n ON n.library = l.pk AND n.parent IS NULL
               ORDER BY lower(o.name), lower(l.name)"#,
            self.token
        )
        .fetch_all(&state.pool)
        .await?;
        let mut positions = HashMap::with_capacity(rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let (position, reset) = match self.positions.get(&r.pk) {
                Some(&(_, p)) => (p, false),
                None => match after.get(&r.id) {
                    Some(&p) if p > r.event_seq => (r.event_seq, true),
                    Some(&p) => (p, false),
                    None => (r.event_seq, false),
                },
            };
            positions.insert(r.pk, (r.id, position));
            out.push(api::FeedLibrary {
                id: r.id,
                organization: r.organization,
                name: r.name,
                root: r.root,
                writable: r.writable,
                trashed: r.trashed,
                position: r.event_seq,
                reset,
            });
        }
        self.positions = positions;
        Ok(out)
    }

    /// The next batch after the positions, collapsed and resolved, in one
    /// snapshot; `None` when there is nothing new. Moves the positions.
    async fn batch(&mut self, state: &AppState) -> sqlx::Result<Option<Batch>> {
        let (libraries, seqs): (Vec<i64>, Vec<i64>) =
            self.positions.iter().map(|(l, (_, s))| (*l, *s)).unzip();
        let mut tx = state.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        // A credential that fell reads nothing from here, before the
        // revocation check ends the stream (`library_access_any` trusts the
        // credential: the auth middleware checked it, once).
        let lifetime = state.settings.get().session_lifetime();
        if crate::db::account::valid_tokens(&mut *tx, &[self.token], lifetime)
            .await?
            .is_empty()
        {
            return Ok(None);
        }
        // The access join again: a library left reads nothing from here.
        let events = sqlx::query!(
            "SELECT e.library, e.seq, e.resource, e.resource_kind, e.action
             FROM unnest($1::bigint[], $2::bigint[]) AS c (library, seq)
             JOIN event e ON e.library = c.library AND e.seq > c.seq
             JOIN library_access_any($3) a ON a.library = e.library
             ORDER BY e.library, e.seq LIMIT $4",
            &libraries,
            &seqs,
            self.token,
            BATCH
        )
        .fetch_all(&mut *tx)
        .await?;
        if events.is_empty() {
            return Ok(None);
        }
        let full = events.len() as i64 == BATCH;
        // Collapsed: each resource's latest event.
        let mut latest: HashMap<(Uuid, api::ResourceKind), api::EventAction> = HashMap::new();
        let mut positions = BTreeMap::new();
        for e in &events {
            let entry = self.positions.get_mut(&e.library).expect("followed");
            entry.1 = e.seq;
            positions.insert(entry.0, e.seq);
            let kind = if e.resource_kind == api::ResourceKind::Library.as_str() {
                api::ResourceKind::Library
            } else {
                api::ResourceKind::Node
            };
            latest.insert((e.resource, kind), api::EventAction::parse(&e.action));
        }
        let of_kind = |kind| -> Vec<Uuid> {
            latest
                .keys()
                .filter(|(_, k)| *k == kind)
                .map(|(id, _)| *id)
                .collect()
        };
        let (library_ids, node_ids) = (
            of_kind(api::ResourceKind::Library),
            of_kind(api::ResourceKind::Node),
        );
        let found_libraries = sqlx::query!(
            r#"SELECT l.uuid, l.name, l.deleted_at IS NOT NULL AS "trashed!"
               FROM library l JOIN library_access_any($1) a ON a.library = l.pk
               WHERE l.uuid = ANY($2)"#,
            self.token,
            &library_ids
        )
        .fetch_all(&mut *tx)
        .await?;
        // A node is gone when trashed (itself or an ancestor), purged, or in a
        // library out of reach.
        let found_nodes = sqlx::query!(
            r#"SELECT n.uuid, l.uuid AS library, p.uuid AS "parent?", n.name, n.is_file,
                      v.uuid AS "current_version?", v.size AS "size?", v.modified_at AS "modified?",
                      node_path(n.pk) AS "path!"
               FROM node n
               JOIN library_access_any($1) a ON a.library = n.library
               JOIN library l ON l.pk = n.library
               LEFT JOIN node p ON p.pk = n.parent
               LEFT JOIN version v ON v.pk = n.current_version
               WHERE n.uuid = ANY($2) AND n.parent IS NOT NULL AND node_reachable(n.pk)"#,
            self.token,
            &node_ids
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let change =
            |id: Uuid, kind: api::ResourceKind, state: Option<api::ChangeState>| api::Change {
                id,
                kind,
                action: latest[&(id, kind)],
                state,
            };
        let mut entries = Vec::with_capacity(latest.len());
        for id in &library_ids {
            let state = found_libraries
                .iter()
                .find(|l| l.uuid == *id)
                .map(|l| api::ChangeState {
                    library: l.uuid,
                    parent: None,
                    name: l.name.clone(),
                    is_file: false,
                    current_version: None,
                    size: None,
                    modified: None,
                    trashed: l.trashed,
                });
            entries.push(change(*id, api::ResourceKind::Library, state));
        }
        // Parents first: by depth in the current tree, so a client never meets
        // a parent it doesn't know (two folders swapped included).
        let mut nodes: Vec<_> = found_nodes.iter().collect();
        nodes.sort_by_key(|n| (n.path.matches('/').count(), n.path.clone()));
        for n in nodes {
            entries.push(change(
                n.uuid,
                api::ResourceKind::Node,
                Some(api::ChangeState {
                    library: n.library,
                    parent: n.parent,
                    name: n.name.clone(),
                    is_file: n.is_file,
                    current_version: n.current_version,
                    size: n.size,
                    modified: n.modified,
                    trashed: false,
                }),
            ));
        }
        // Then those gone.
        for id in node_ids
            .iter()
            .filter(|id| !found_nodes.iter().any(|n| n.uuid == **id))
        {
            entries.push(change(*id, api::ResourceKind::Node, None));
        }
        Ok(Some(Batch {
            changes: api::Changes { entries, positions },
            full,
        }))
    }
}

/// One stream's life: `libraries`, the catch-up, then live until the client
/// goes, its credential falls, or the database fails (the client reconnects).
pub async fn run(
    state: AppState,
    who: Credential,
    after: BTreeMap<Uuid, i64>,
    registration: Registration,
    out: mpsc::Sender<SseEvent>,
) {
    if let Err(e) = live(&state, &who, &after, &registration, &out).await {
        tracing::warn!(error = %e, account = %who.account_uuid, "event stream ended");
    }
}

async fn live(
    state: &AppState,
    who: &Credential,
    after: &BTreeMap<Uuid, i64>,
    registration: &Registration,
    out: &mpsc::Sender<SseEvent>,
) -> sqlx::Result<()> {
    let mut feed = Feed {
        token: who.token,
        positions: HashMap::new(),
    };
    let signal = &registration.signal;
    let mut access_changed = true;
    // The client's positions count once, at the start: a library gained later
    // is new to it, followed from its last event.
    let mut after = after.clone();
    loop {
        if signal.closed.load(Ordering::Relaxed) {
            return Ok(());
        }
        // The libraries first, and the registry told, then the read: an event
        // committed in between finds the set updated, or is read below.
        if access_changed {
            let libraries = feed.libraries(state, &after).await?;
            after.clear();
            state
                .events
                .set_libraries(registration.id, feed.positions.keys().copied().collect());
            let event = SseEvent::default()
                .event("libraries")
                .json_data(&libraries)
                .expect("serializable");
            if out.send(event).await.is_err() {
                return Ok(());
            }
        }
        while let Some(batch) = feed.batch(state).await? {
            let event = SseEvent::default()
                .event("changes")
                .json_data(&batch.changes)
                .expect("serializable");
            if out.send(event).await.is_err() {
                return Ok(());
            }
            if !batch.full {
                break;
            }
        }
        tokio::select! {
            _ = signal.notify.notified() => {}
            _ = out.closed() => return Ok(()),
        }
        access_changed = signal.access.swap(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the stream was woken (a pending permit counts).
    async fn woken(r: &Registration) -> bool {
        tokio::time::timeout(Duration::from_millis(50), r.signal.notify.notified())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn wakes_the_streams_concerned() {
        let registry = Registry::new(INTERVAL);
        let photos = registry.register(1, 10).unwrap();
        let docs = registry.register(2, 20).unwrap();
        registry.set_libraries(photos.id, HashSet::from([100]));
        registry.set_libraries(docs.id, HashSet::from([200]));

        registry.wake(&[100], &[]);
        assert!(woken(&photos).await && !woken(&docs).await);
        // An access change: woken, and told so.
        registry.wake(&[], &[20]);
        assert!(!woken(&photos).await && woken(&docs).await);
        assert!(docs.signal.access.swap(false, Ordering::Relaxed));
        assert!(!photos.signal.access.load(Ordering::Relaxed));
        // Many wake-ups while busy: one pending.
        for _ in 0..100 {
            registry.wake(&[100], &[]);
        }
        assert!(woken(&photos).await && !woken(&photos).await);
        // A credential fallen: closed.
        registry.close(&HashSet::from([2]));
        assert!(woken(&photos).await && photos.signal.closed.load(Ordering::Relaxed));
        assert!(!docs.signal.closed.load(Ordering::Relaxed));
        assert_eq!(registry.tokens(), [1, 2]);
    }

    #[tokio::test]
    async fn ten_per_credential_and_freed_on_drop() {
        let registry = Registry::new(INTERVAL);
        let mut held: Vec<_> = (0..STREAMS_PER_CREDENTIAL)
            .map(|_| registry.register(1, 10).unwrap())
            .collect();
        assert!(registry.register(1, 10).is_none());
        assert!(
            registry.register(2, 10).is_some(),
            "per credential, not account"
        );
        held.pop();
        assert!(registry.register(1, 10).is_some());
        drop(held);
        assert_eq!(registry.lock().len(), 0);
    }
}
