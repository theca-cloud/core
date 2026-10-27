-- Sprint 06, wider dedup (docs/dedup.md, ADR 0002).

-- A version's author is the account, not its membership: leaving an
-- organization deletes the membership row, which cleared the author. Cleared
-- now only when the account itself is deleted.
ALTER TABLE version DROP CONSTRAINT version_author;
UPDATE version v SET author = u.account FROM "user" u WHERE u.pk = v.author;
ALTER TABLE version
    ADD CONSTRAINT version_author FOREIGN KEY (author) REFERENCES account ON DELETE SET NULL;

-- The libraries an upload into `library`, with the credential `token`, may
-- reuse chunks from: the library itself, the libraries sharing a namespace
-- with it, and every library the credential can access in the library's
-- organization, trash included (library_access_any: a member can restore and
-- read a trashed library, so reusing its chunks reveals nothing). The callers
-- add the storage and the chunk's own conditions. One definition for the
-- lookup and the commit check, so a chunk a lookup hands out is never refused.
CREATE FUNCTION dedup_scope(library bigint, token bigint)
RETURNS TABLE (library bigint) LANGUAGE sql STABLE AS $$
    SELECT $1
    UNION
    SELECT other.library FROM library_namespace mine
    JOIN library_namespace other ON other.namespace = mine.namespace
    WHERE mine.library = $1
    UNION
    SELECT a.library FROM library_access_any($2) a
    JOIN library l ON l.pk = $1
    WHERE a.organization = l.owner
$$;
