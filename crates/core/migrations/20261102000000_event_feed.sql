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
