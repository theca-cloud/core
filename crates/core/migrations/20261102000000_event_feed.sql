-- Sprint 08, the event feed (docs/decisions.md, Sync; ADR 0003).

-- The number of the library's last event. Raised by the server (actions::commit)
-- just before a transaction that wrote events commits: the row lock it takes
-- lasts until the commit, so commits into one library number their events in
-- commit order, without gaps (a rollback undoes the increment).
ALTER TABLE library ADD COLUMN event_seq bigint NOT NULL DEFAULT 0;

-- event : one change to files or a library, numbered in its library. A client
-- reads the events after its position in each library it follows; the server
-- resolves each resource to its current state when read, so an event stores
-- what happened, never the state (no name, no path). Inserted only by
-- actions::commit.
CREATE TABLE event (
    pk bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Who can see the event (library_access_any) and the order it belongs to.
    -- A purged library takes its events: nobody can see them anymore.
    library bigint NOT NULL REFERENCES library ON DELETE CASCADE,
    seq bigint NOT NULL,
    -- The node or the library concerned. No FK: a node's last events (trashed,
    -- purged) are how an offline client learns it's gone, so they outlive it.
    resource uuid NOT NULL,
    resource_kind text NOT NULL,
    action text NOT NULL,
    -- Who; NULL for a job, or once the account is deleted.
    actor bigint REFERENCES account ON DELETE SET NULL,
    at timestamptz NOT NULL DEFAULT now(),
    -- Also the feed's index: the events of a library after a position.
    CONSTRAINT event_seq_unique UNIQUE (library, seq),
    CONSTRAINT event_seq_positive CHECK (seq > 0),
    CONSTRAINT event_action CHECK (
        (resource_kind = 'node' AND action IN ('created', 'versioned', 'touched',
            'version_restored', 'renamed', 'moved', 'trashed', 'restored', 'purged'))
        OR (resource_kind = 'library' AND action IN ('renamed', 'trashed', 'restored'))
    )
);
CREATE INDEX event_actor ON event (actor);

-- Sprint 09, the sync client's browser login (docs/decisions.md, Authentication).

-- login_flow : a device logging in through the browser. The device starts it
-- (a name, a code it shows) and polls with a secret; the account approves it on
-- the web app from a full session; the poll that finds it approved creates the
-- device's app password, returns it, and deletes the flow, in one transaction:
-- the app password is never stored here. Valid for 15 minutes from creation
-- (checked by the queries; the cleanup job deletes expired ones).
CREATE TABLE login_flow (
    pk bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    uuid uuid NOT NULL UNIQUE DEFAULT uuidv7(),
    -- The device's name: the app password's.
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    -- Shown by the device and on the approval page, so the person approving
    -- checks it's their device's flow.
    code text NOT NULL,
    -- SHA-256 of the poll secret, which only the device knows.
    poll_hash bytea NOT NULL UNIQUE CHECK (length(poll_hash) = 32),
    -- Who approved it; NULL while pending.
    account bigint REFERENCES account ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX login_flow_account ON login_flow (account);
