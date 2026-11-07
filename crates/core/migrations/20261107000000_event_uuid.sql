-- Sprint 12, the cursor's event (docs/decisions.md, Sync).

-- An event's ID: a client sends it back with its position, so that a database
-- restored from a backup, whose numbers past the restore point are reused by
-- new events, is told apart. Existing events get an ID carrying their own time.
-- No unique index: it's only read through (library, seq), already unique.
ALTER TABLE event ADD COLUMN uuid uuid;
UPDATE event SET uuid = uuidv7(at - clock_timestamp());
ALTER TABLE event ALTER COLUMN uuid SET DEFAULT uuidv7(), ALTER COLUMN uuid SET NOT NULL;
