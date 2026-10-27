//! Invariants the database enforces by itself (constraints and triggers). Each
//! test checks that the database accepts what's valid and refuses what isn't,
//! with the expected constraint, so a schema change can't silently drop a
//! guarantee.

use std::time::Duration;

use sqlx::PgPool;

/// A small world, with explicit keys:
///
/// - organization 1: users 1 (owner), 2 (member), 4 (invited, not joined)
/// - organization 2: user 3 (owner)
/// - storages: 1 shared, 2 reserved to organization 1, 3 reserved to organization 2
/// - libraries: 1 and 2 (organization 1), 3 (organization 2)
/// - nodes of library 1: root 1, folders 2 (in 1) and 3 (in 2), file 4 (in 1),
///   folders 8 and 9 (in 1); roots 5 (library 2) and 6 (library 3), file 7 (in 6)
/// - versions: 1 of file 4 (current), 2 of file 7
/// - chunks: 1 (organization 1, ready, in version 1), 2 (organization 2, ready),
///   3 (organization 1, pending)
async fn fixture(pool: &PgPool) {
    sqlx::raw_sql(
        r#"
        BEGIN;
        INSERT INTO account (pk, email) OVERRIDING SYSTEM VALUE
            VALUES (1, 'a@x.ch'), (2, 'b@x.ch'), (3, 'c@x.ch'), (4, 'd@x.ch');
        INSERT INTO organization (pk, name, owner) OVERRIDING SYSTEM VALUE VALUES (1, 'one', 1), (2, 'two', 3);
        INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, now()), (2, 2, 1, now()), (3, 3, 2, now()), (4, 4, 1, NULL);
        INSERT INTO storage (pk, organization, name, implementation, details) OVERRIDING SYSTEM VALUE
            VALUES (1, NULL, 'shared', 'filesystem', '{}'),
                   (2, 1, 'one', 'filesystem', '{}'),
                   (3, 2, 'two', 'filesystem', '{}');
        INSERT INTO library (pk, name, owner, storage) OVERRIDING SYSTEM VALUE
            VALUES (1, 'lib1', 1, 1), (2, 'lib2', 1, 1), (3, 'lib3', 2, 1);
        INSERT INTO node (pk, organization, library, parent, name, is_file) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, NULL, '', false), (2, 1, 1, 1, 'a', false), (3, 1, 1, 2, 'b', false),
                   (4, 1, 1, 1, 'f.txt', true), (8, 1, 1, 1, 'x', false), (9, 1, 1, 1, 'y', false),
                   (5, 1, 2, NULL, '', false), (6, 2, 3, NULL, '', false), (7, 2, 3, 6, 'g.txt', true);
        INSERT INTO version (pk, organization, file, size) OVERRIDING SYSTEM VALUE VALUES (1, 1, 4, 0), (2, 2, 7, 0);
        UPDATE node SET current_version = 1 WHERE pk = 4;
        UPDATE node SET current_version = 2 WHERE pk = 7;
        INSERT INTO chunk (pk, organization, library, hash, storage, plain_size, stored_size, compression,
                           stored_sha256, uploaded_at)
            OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, '\x00000000000000000000000000000001', 1, 1, 1, 'none', decode(repeat('00', 32), 'hex'), now()),
                   (2, 2, 3, '\x00000000000000000000000000000002', 1, 1, 1, 'none', decode(repeat('00', 32), 'hex'), now());
        INSERT INTO chunk (pk, organization, library, hash, storage, plain_size) OVERRIDING SYSTEM VALUE
            VALUES (3, 1, 1, '\x00000000000000000000000000000003', 1, 7);
        INSERT INTO chunk_in_version (organization, version, chunk, position) VALUES (1, 1, 1, 0);
        COMMIT;
        -- Explicit keys above: move every identity sequence past them.
        DO $$
        DECLARE t text;
        BEGIN
            FOR t IN SELECT table_name FROM information_schema.columns
                     WHERE table_schema = 'public' AND column_name = 'pk' AND is_identity = 'YES'
            LOOP
                PERFORM setval(pg_get_serial_sequence(quote_ident(t), 'pk'), 1000);
            END LOOP;
        END $$;
        "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Runs `sql` and returns the name of the constraint that refused it, if any.
/// After a refusal, rolls back on the same connection: a multi-statement `sql`
/// (`BEGIN; ...; COMMIT;`) failing midway would otherwise return the connection
/// to the pool inside an aborted transaction.
async fn refused_by(pool: &PgPool, sql: &str) -> Option<String> {
    let mut conn = pool.acquire().await.unwrap();
    let result = sqlx::raw_sql(sql).execute(&mut *conn).await;
    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
    match result {
        Ok(_) => None,
        Err(sqlx::Error::Database(e)) => Some(e.constraint().map_or_else(
            || format!("<no constraint: {}>", e.message()),
            str::to_owned,
        )),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

async fn assert_refused(pool: &PgPool, constraint: &str, sql: &str) {
    assert_eq!(
        refused_by(pool, sql).await.as_deref(),
        Some(constraint),
        "{sql}"
    );
}

async fn assert_accepted(pool: &PgPool, sql: &str) {
    assert_eq!(refused_by(pool, sql).await, None, "{sql}");
}

// Storage organization (triggers 1-4).

#[sqlx::test]
async fn library_storage_is_shared_or_its_organizations(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO library (name, owner, storage) VALUES";
    assert_accepted(&pool, &format!("{insert} ('a', 1, 1)")).await;
    assert_accepted(&pool, &format!("{insert} ('b', 1, 2)")).await;
    let c = "library_storage_organization";
    assert_refused(&pool, c, &format!("{insert} ('c', 1, 3)")).await;
    assert_refused(&pool, c, "UPDATE library SET storage = 3 WHERE pk = 1").await;
}

#[sqlx::test]
async fn default_storage_is_shared_or_its_organizations(pool: PgPool) {
    fixture(&pool).await;
    assert_accepted(
        &pool,
        "UPDATE organization SET default_storage = 1 WHERE pk = 1",
    )
    .await;
    assert_accepted(
        &pool,
        "UPDATE organization SET default_storage = 2 WHERE pk = 1",
    )
    .await;
    assert_refused(
        &pool,
        "organization_default_storage",
        "UPDATE organization SET default_storage = 3 WHERE pk = 1",
    )
    .await;
}

#[sqlx::test]
async fn chunk_storage_is_shared_or_its_organizations(pool: PgPool) {
    fixture(&pool).await;
    let chunk = |storage: i32, hash: u8| {
        format!(
            "INSERT INTO chunk (organization, hash, storage, plain_size) VALUES (1, '\\x{hash:032x}', {storage}, 1)"
        )
    };
    assert_accepted(&pool, &chunk(1, 10)).await;
    assert_accepted(&pool, &chunk(2, 11)).await;
    assert_refused(&pool, "chunk_storage_organization", &chunk(3, 12)).await;
}

#[sqlx::test]
async fn storage_organization_never_changes(pool: PgPool) {
    fixture(&pool).await;
    let c = "storage_organization_immutable";
    assert_refused(&pool, c, "UPDATE storage SET organization = 1 WHERE pk = 1").await;
    assert_refused(
        &pool,
        c,
        "UPDATE storage SET organization = NULL WHERE pk = 2",
    )
    .await;
    assert_refused(&pool, c, "UPDATE storage SET organization = 2 WHERE pk = 2").await;
    assert_accepted(&pool, "UPDATE storage SET name = 'renamed' WHERE pk = 2").await;
}

#[sqlx::test]
async fn one_default_storage(pool: PgPool) {
    fixture(&pool).await;
    assert_accepted(&pool, "UPDATE storage SET is_default = true WHERE pk = 1").await;
    assert_refused(
        &pool,
        "storage_one_default",
        "UPDATE storage SET is_default = true WHERE pk = 2",
    )
    .await;
}

// Organizations, accounts, identities, tokens.

#[sqlx::test]
async fn organization_owner_has_joined(pool: PgPool) {
    fixture(&pool).await;
    assert_refused(
        &pool,
        "organization_owner",
        "UPDATE organization SET owner = 4 WHERE pk = 1",
    )
    .await;
    assert_refused(
        &pool,
        "organization_owner",
        "UPDATE organization SET owner = 3 WHERE pk = 1",
    )
    .await;
    assert_refused(
        &pool,
        "organization_owner",
        "UPDATE \"user\" SET joined_at = NULL WHERE pk = 1",
    )
    .await;
}

#[sqlx::test]
async fn account_email_unique_ignoring_case(pool: PgPool) {
    fixture(&pool).await;
    assert_refused(
        &pool,
        "account_email",
        "INSERT INTO account (email) VALUES ('A@X.CH')",
    )
    .await;
}

#[sqlx::test]
async fn personal_organizations(pool: PgPool) {
    fixture(&pool).await;
    // Organization 1 is owned by account 1: it can be account 1's personal one, not account 2's.
    assert_refused(
        &pool,
        "organization_personal_owner",
        "UPDATE organization SET personal_of = 2 WHERE pk = 1",
    )
    .await;
    assert_accepted(
        &pool,
        "UPDATE organization SET personal_of = 1 WHERE pk = 1",
    )
    .await;
}

#[sqlx::test]
async fn a_leaving_author_keeps_their_versions(pool: PgPool) {
    // The author is the account (sprint 06): leaving the organization deletes
    // the membership, not the author; deleting the account clears it.
    fixture(&pool).await;
    let author = || async {
        sqlx::query_scalar::<_, Option<i64>>("SELECT author FROM version WHERE pk = 1")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    sqlx::raw_sql(r#"UPDATE version SET author = 2 WHERE pk = 1; DELETE FROM "user" WHERE pk = 2"#)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        author().await,
        Some(2),
        "the membership gone, the author stays"
    );
    sqlx::raw_sql("DELETE FROM account WHERE pk = 2")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(author().await, None, "the account gone, the author goes");
    assert_refused(
        &pool,
        "version_author",
        "UPDATE version SET author = 999 WHERE pk = 1",
    )
    .await;
}

#[sqlx::test]
async fn leaving_an_organization_drops_its_identities(pool: PgPool) {
    fixture(&pool).await;
    // Account 2 (user 2, organization 1) logs in through organization 1's
    // provider and an instance-wide one.
    sqlx::raw_sql(
        r#"INSERT INTO identity_provider (pk, organization, name, issuer, client_id, client_secret_file, client_secret_encrypted)
               OVERRIDING SYSTEM VALUE VALUES (1, 1, 'one sso', 'https://one', 'c', NULL, 'sealed'),
                                              (2, NULL, 'google', 'https://google', 'c', '/s', NULL);
           INSERT INTO identity (account, kind, provider, details)
               VALUES (2, 'oidc', 1, '{"subject": "s1"}'), (2, 'oidc', 2, '{"subject": "s2"}');"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(r#"DELETE FROM "user" WHERE pk = 2"#)
        .execute(&pool)
        .await
        .unwrap();
    let providers: Vec<i64> =
        sqlx::query_scalar("SELECT provider FROM identity WHERE account = 2 AND kind = 'oidc'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(providers, [2], "only the instance-wide identity is left");
}

#[sqlx::test]
async fn identity_details(pool: PgPool) {
    fixture(&pool).await;
    // Providers: instance-wide (1) and organization 1's (2); names unique per organization.
    assert_accepted(
        &pool,
        "INSERT INTO identity_provider (pk, organization, name, issuer, client_id, client_secret_file, client_secret_encrypted) \
         OVERRIDING SYSTEM VALUE VALUES (1, NULL, 'SSO', 'https://i', 'c', '/s', NULL), (2, 1, 'SSO', 'https://j', 'c', NULL, 'sealed')",
    )
    .await;
    assert_refused(
        &pool,
        "identity_provider_name",
        "INSERT INTO identity_provider (organization, name, issuer, client_id, client_secret_file) \
         VALUES (NULL, 'sso', 'https://k', 'c', '/s')",
    )
    .await;
    // An organization's provider never reads a server file; a secret either way, not both.
    let provider = "INSERT INTO identity_provider (organization, name, issuer, client_id, client_secret_file, client_secret_encrypted) VALUES";
    assert_refused(
        &pool,
        "identity_provider_secret_file",
        &format!("{provider} (1, 'p1', 'https://p', 'c', '/etc/passwd', NULL)"),
    )
    .await;
    assert_refused(
        &pool,
        "identity_provider_secret",
        &format!("{provider} (NULL, 'p2', 'https://p', 'c', '/s', 'sealed')"),
    )
    .await;
    assert_refused(
        &pool,
        "identity_provider_secret",
        &format!("{provider} (NULL, 'p3', 'https://p', 'c', NULL, NULL)"),
    )
    .await;

    let insert = "INSERT INTO identity (account, kind, provider, details) VALUES";
    assert_accepted(
        &pool,
        &format!(r#"{insert} (1, 'password', NULL, '{{"hash": "h"}}')"#),
    )
    .await;
    assert_accepted(
        &pool,
        &format!(r#"{insert} (1, 'oidc', 1, '{{"subject": "s"}}')"#),
    )
    .await;
    assert_accepted(
        &pool,
        &format!(
            r#"{insert} (1, 'totp', NULL, '{{"secret": "x", "confirmed": false, "last_step": 0}}')"#
        ),
    )
    .await;
    assert_accepted(
        &pool,
        &format!(r#"{insert} (2, 'oidc', 2, '{{"subject": "s"}}')"#),
    )
    .await; // other provider
    assert_refused(
        &pool,
        "identity_password_details",
        &format!("{insert} (2, 'password', NULL, '{{}}')"),
    )
    .await;
    assert_refused(
        &pool,
        "identity_oidc_details",
        &format!("{insert} (2, 'oidc', 1, '{{}}')"),
    )
    .await;
    assert_refused(
        &pool,
        "identity_totp_details",
        &format!(r#"{insert} (2, 'totp', NULL, '{{"secret": "x", "last_step": 0}}')"#),
    )
    .await;
    assert_refused(
        &pool,
        "identity_provider_oidc",
        &format!(r#"{insert} (2, 'oidc', NULL, '{{"subject": "t"}}')"#),
    )
    .await;
    assert_refused(
        &pool,
        "identity_provider_oidc",
        &format!(r#"{insert} (2, 'password', 1, '{{"hash": "h"}}')"#),
    )
    .await;
    assert_refused(
        &pool,
        "identity_password",
        &format!(r#"{insert} (1, 'password', NULL, '{{"hash": "other"}}')"#),
    )
    .await;
    assert_refused(
        &pool,
        "identity_totp",
        &format!(
            r#"{insert} (1, 'totp', NULL, '{{"secret": "y", "confirmed": false, "last_step": 0}}')"#
        ),
    )
    .await;
    assert_refused(
        &pool,
        "identity_oidc",
        &format!(r#"{insert} (2, 'oidc', 1, '{{"subject": "s"}}')"#),
    )
    .await;
}

#[sqlx::test]
async fn token_scopes(pool: PgPool) {
    fixture(&pool).await;
    let token = |prefix: &str, usage: &str, organization: &str, library: &str, read_only: bool| {
        format!(
            "INSERT INTO token (usage, resource, prefix, hash, name, organization, library, read_only) \
             VALUES ('{usage}', uuidv7(), '{prefix}', decode(repeat('00', 32), 'hex'), 'x', \
                     {organization}, {library}, {read_only})"
        )
    };
    // Restricted session; app passwords scoped to an organization, a library, read-only.
    assert_accepted(&pool, &token("s1", "session", "1", "NULL", false)).await;
    assert_accepted(&pool, &token("a1", "app_password", "1", "NULL", true)).await;
    assert_accepted(&pool, &token("a2", "app_password", "1", "1", false)).await;
    let refusals = [
        (
            "token_organization_scope",
            token("i1", "invitation", "1", "NULL", false),
        ),
        (
            "token_library_scope",
            token("s2", "session", "1", "1", false),
        ),
        (
            "token_library_scope",
            token("a3", "app_password", "NULL", "1", false),
        ),
        (
            "token_read_only",
            token("s3", "session", "NULL", "NULL", true),
        ),
        // Library 1 belongs to organization 1.
        (
            "token_library",
            token("a4", "app_password", "2", "1", false),
        ),
    ];
    for (constraint, sql) in refusals {
        assert_refused(&pool, constraint, &sql).await;
    }
}

#[sqlx::test]
async fn token_revocation_has_a_reason(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO token (usage, resource, name, prefix, hash, revoked_at, revocation_reason) \
                  VALUES ('session', uuidv7(), 'x', 'p1', '\\x0000000000000000000000000000000000000000000000000000000000000000'";
    assert_refused(
        &pool,
        "token_revocation_reason",
        &format!("{insert}, now(), NULL)"),
    )
    .await;
    assert_accepted(&pool, &format!("{insert}, now(), 'logout')")).await;
}

// Libraries, members, namespaces.

#[sqlx::test]
async fn library_names(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO library (name, owner, storage) VALUES";
    assert_refused(&pool, "library_name", &format!("{insert} ('LIB1', 1, 1)")).await;
    assert_accepted(&pool, &format!("{insert} ('lib1', 2, 1)")).await; // another organization
    assert_refused(
        &pool,
        "library_name_valid",
        &format!("{insert} ('con', 1, 1)"),
    )
    .await;
}

#[sqlx::test]
async fn library_members_belong_to_the_organization(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO library_member (organization, library, \"user\", role) VALUES";
    assert_accepted(&pool, &format!("{insert} (1, 1, 2, 'write')")).await;
    assert_accepted(&pool, &format!("{insert} (1, 1, 4, 'read')")).await; // invited user
    assert_refused(
        &pool,
        "library_member_user",
        &format!("{insert} (1, 1, 3, 'read')"),
    )
    .await;
    assert_refused(
        &pool,
        "library_member_library",
        &format!("{insert} (1, 3, 1, 'read')"),
    )
    .await;
}

#[sqlx::test]
async fn namespaces_stay_in_their_organization(pool: PgPool) {
    fixture(&pool).await;
    assert_accepted(
        &pool,
        "INSERT INTO namespace (pk, organization, name) OVERRIDING SYSTEM VALUE \
         VALUES (1, 1, 'general'), (2, 2, 'other')",
    )
    .await;
    assert_refused(
        &pool,
        "namespace_name",
        "INSERT INTO namespace (organization, name) VALUES (1, 'GENERAL')",
    )
    .await;
    let insert = "INSERT INTO library_namespace (organization, library, namespace) VALUES";
    assert_accepted(&pool, &format!("{insert} (1, 1, 1)")).await;
    assert_refused(
        &pool,
        "library_namespace_namespace",
        &format!("{insert} (1, 2, 2)"),
    )
    .await;
    assert_refused(
        &pool,
        "library_namespace_library",
        &format!("{insert} (1, 3, 1)"),
    )
    .await;
}

// Tree.

#[sqlx::test]
async fn node_rules(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO node (organization, library, parent, name, is_file) VALUES";
    assert_refused(
        &pool,
        "node_name",
        &format!("{insert} (1, 1, 1, 'F.TXT', true)"),
    )
    .await;
    assert_refused(
        &pool,
        "node_name_valid",
        &format!("{insert} (1, 1, 1, 'con.txt', true)"),
    )
    .await;
    // An empty name outside the root breaks two rules; Postgres reports one of them.
    let refused = refused_by(&pool, &format!("{insert} (1, 1, 1, '', false)")).await;
    assert!(
        matches!(
            refused.as_deref(),
            Some("node_root_unnamed" | "node_name_valid")
        ),
        "{refused:?}"
    );
    assert_refused(
        &pool,
        "node_one_root",
        &format!("{insert} (1, 1, NULL, '', false)"),
    )
    .await;
    assert_refused(
        &pool,
        "node_parent",
        &format!("{insert} (1, 1, 4, 'in-a-file', false)"),
    )
    .await;
    assert_refused(
        &pool,
        "node_parent",
        &format!("{insert} (1, 1, 5, 'other-library', false)"),
    )
    .await;
    assert_refused(
        &pool,
        "node_library",
        &format!("{insert} (2, 1, 1, 'wrong-org', false)"),
    )
    .await;
    // A trashed node frees its name (a folder takes it: a bare file would have no version).
    assert_accepted(&pool, "UPDATE node SET deleted_at = now() WHERE pk = 4").await;
    assert_accepted(&pool, &format!("{insert} (1, 1, 1, 'F.TXT', false)")).await;
}

#[sqlx::test]
async fn moves_never_create_cycles(pool: PgPool) {
    fixture(&pool).await;
    assert_refused(
        &pool,
        // The trigger runs before the CHECK (node_not_own_parent) and catches this too.
        "node_no_cycle",
        "UPDATE node SET parent = 2 WHERE pk = 2",
    )
    .await;
    assert_refused(
        &pool,
        "node_no_cycle",
        "UPDATE node SET parent = 3 WHERE pk = 2",
    )
    .await;
    assert_accepted(&pool, "UPDATE node SET parent = 9 WHERE pk = 2").await;
}

/// Two concurrent moves, "8 under 9" and "9 under 8": the second waits for the
/// first (library lock), then sees it and refuses the cycle.
#[sqlx::test]
async fn concurrent_moves_never_create_cycles(pool: PgPool) {
    fixture(&pool).await;
    let mut first = pool.begin().await.unwrap();
    sqlx::query("UPDATE node SET parent = 9 WHERE pk = 8")
        .execute(&mut *first)
        .await
        .unwrap();

    let second = tokio::spawn({
        let pool = pool.clone();
        // Directly on the pool: refused_by holds a borrowed connection, which a
        // spawned task can't.
        async move {
            match sqlx::query("UPDATE node SET parent = 8 WHERE pk = 9")
                .execute(&pool)
                .await
            {
                Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second move should wait for the first"
    );

    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap().as_deref(), Some("node_no_cycle"));
}

// Versions and chunks.

#[sqlx::test]
async fn version_rules(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO version (organization, file, parent_version, size) VALUES";
    assert_refused(&pool, "version_file", &format!("{insert} (1, 2, NULL, 0)")).await; // a folder
    assert_refused(&pool, "version_file", &format!("{insert} (2, 4, NULL, 0)")).await; // wrong org
    assert_refused(&pool, "version_parent", &format!("{insert} (1, 4, 2, 0)")).await; // other file
    assert_refused(
        &pool,
        "node_current_version",
        "BEGIN; UPDATE node SET current_version = 2 WHERE pk = 4; COMMIT;",
    )
    .await;
}

#[sqlx::test]
async fn versions_only_reference_ready_chunks_of_their_organization(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO chunk_in_version (organization, version, chunk, position) VALUES";
    let c = "chunk_in_version_chunk";
    assert_refused(&pool, c, &format!("{insert} (1, 1, 2, 1)")).await; // other organization
    assert_refused(&pool, c, &format!("{insert} (1, 1, 3, 1)")).await; // pending
    assert_refused(
        &pool,
        "chunk_in_version_version",
        &format!("{insert} (2, 1, 2, 1)"),
    )
    .await;
    // A referenced chunk can't be tombstoned; an unreferenced one can.
    assert_refused(
        &pool,
        c,
        "UPDATE chunk SET deleting_at = now() WHERE pk = 1",
    )
    .await;
    assert_accepted(&pool, "UPDATE chunk SET deleting_at = now() WHERE pk = 2").await;
}

#[sqlx::test]
async fn chunk_declarations(pool: PgPool) {
    fixture(&pool).await;
    let declare = |size: i64, sha_bytes: usize| {
        format!(
            "UPDATE chunk SET stored_size = {size}, compression = 'lz4', \
             stored_sha256 = decode(repeat('00', {sha_bytes}), 'hex') WHERE pk = 3"
        )
    };
    assert_refused(
        &pool,
        "chunk_uploaded_declared",
        "UPDATE chunk SET uploaded_at = now() WHERE pk = 3",
    )
    .await;
    assert_refused(
        &pool,
        "chunk_declared",
        "UPDATE chunk SET stored_size = 5 WHERE pk = 3",
    )
    .await;
    assert_refused(&pool, "chunk_stored_size", &declare(8, 32)).await; // plain_size is 7
    assert_refused(&pool, "chunk_stored_sha256_check", &declare(5, 16)).await;
    assert_accepted(&pool, &declare(5, 32)).await;
    assert_accepted(&pool, "UPDATE chunk SET uploaded_at = now() WHERE pk = 3").await;
    assert_refused(
        &pool,
        "chunk_plain_size",
        "INSERT INTO chunk (organization, hash, storage, plain_size) \
         VALUES (1, '\\x000000000000000000000000000000aa', 1, 8388609)",
    )
    .await;
    assert_refused(
        &pool,
        "organization_quota",
        "UPDATE organization SET quota = -1 WHERE pk = 1",
    )
    .await;
    // declared_by is a user of the chunk's organization (user 3 is in organization 2).
    assert_refused(
        &pool,
        "chunk_organization_declared_by_fkey",
        "UPDATE chunk SET declared_by = 3 WHERE pk = 3",
    )
    .await;
    assert_accepted(&pool, "UPDATE chunk SET declared_by = 2 WHERE pk = 3").await;
}

// Background jobs.

#[sqlx::test]
async fn job_runs(pool: PgPool) {
    fixture(&pool).await;
    let insert = "INSERT INTO job_run (job, status, finished_at, error) VALUES";
    assert_accepted(&pool, &format!("{insert} ('gc', 'running', NULL, NULL)")).await;
    assert_refused(
        &pool,
        "job_run_finished",
        &format!("{insert} ('gc', 'success', NULL, NULL)"),
    )
    .await;
    assert_refused(
        &pool,
        "job_run_failure_error",
        &format!("{insert} ('gc', 'failure', now(), NULL)"),
    )
    .await;
}

// Closed lists, formats and counters.

#[sqlx::test]
async fn closed_lists_and_formats(pool: PgPool) {
    fixture(&pool).await;
    let token = |usage: &str, prefix: &str, hash_bytes: usize| {
        format!(
            "INSERT INTO token (usage, resource, name, prefix, hash) VALUES ('{usage}', uuidv7(), 'x', '{prefix}', '\\x{}')",
            "00".repeat(hash_bytes)
        )
    };
    assert_accepted(&pool, &token("session", "p1", 32)).await;
    assert_refused(&pool, "token_usage_check", &token("magic", "p2", 32)).await;
    assert_refused(&pool, "token_hash_check", &token("session", "p3", 16)).await;
    assert_refused(&pool, "token_prefix_key", &token("session", "p1", 32)).await;
    assert_refused(
        &pool,
        "identity_kind_check",
        "INSERT INTO identity (account, kind, details) VALUES (1, 'ldap', '{}')",
    )
    .await;
    assert_refused(
        &pool,
        "library_member_role_check",
        "INSERT INTO library_member (organization, library, \"user\", role) VALUES (1, 1, 2, 'admin')",
    )
    .await;
    assert_refused(
        &pool,
        "chunk_hash_check",
        "INSERT INTO chunk (organization, hash, storage, plain_size) VALUES (1, '\\x00', 1, 1)",
    )
    .await;
    assert_refused(
        &pool,
        "chunk_compression_check",
        "UPDATE chunk SET stored_size = 1, compression = 'zip', stored_sha256 = decode(repeat('00', 32), 'hex') WHERE pk = 3",
    )
    .await;
    assert_refused(
        &pool,
        "job_run_status_check",
        "INSERT INTO job_run (job, status, finished_at) VALUES ('gc', 'paused', now())",
    )
    .await;
    assert_refused(
        &pool,
        "job_run_items_check",
        "INSERT INTO job_run (job, items) VALUES ('gc', -1)",
    )
    .await;
    assert_refused(
        &pool,
        "email_attempts_check",
        "INSERT INTO email (recipient, template, params, attempts) VALUES ('a@x.ch', 't', '{}', -1)",
    )
    .await;
}

// Memberships.

#[sqlx::test]
async fn one_membership_each(pool: PgPool) {
    fixture(&pool).await;
    assert_refused(
        &pool,
        "user_account_organization_key",
        "INSERT INTO \"user\" (account, organization) VALUES (2, 1)",
    )
    .await;
    let member = "INSERT INTO library_member (organization, library, \"user\", role) VALUES (1, 1, 2, 'read')";
    assert_accepted(&pool, member).await;
    assert_refused(&pool, "library_member_library_user_key", member).await;
    assert_accepted(
        &pool,
        "INSERT INTO namespace (pk, organization, name) OVERRIDING SYSTEM VALUE VALUES (1, 1, 'n')",
    )
    .await;
    let in_namespace =
        "INSERT INTO library_namespace (organization, library, namespace) VALUES (1, 1, 1)";
    assert_accepted(&pool, in_namespace).await;
    assert_refused(
        &pool,
        "library_namespace_library_namespace_key",
        in_namespace,
    )
    .await;
}

// Tree and versions: remaining rules.

#[sqlx::test]
async fn tree_remaining_rules(pool: PgPool) {
    fixture(&pool).await;
    // A file always has a current version, checked at commit: inserted
    // without one and given one in the same transaction is fine; left
    // without, or losing it, is refused; gone by the commit passes.
    assert_accepted(
        &pool,
        "BEGIN; \
         INSERT INTO node (pk, organization, library, parent, name, is_file) OVERRIDING SYSTEM VALUE \
             VALUES (60, 1, 1, 1, 'new.txt', true); \
         INSERT INTO version (pk, organization, file, size) OVERRIDING SYSTEM VALUE VALUES (60, 1, 60, 0); \
         UPDATE node SET current_version = 60 WHERE pk = 60; \
         COMMIT;",
    )
    .await;
    assert_refused(
        &pool,
        "node_file_has_version",
        "BEGIN; INSERT INTO node (organization, library, parent, name, is_file) VALUES (1, 1, 1, 'bare.txt', true); COMMIT;",
    )
    .await;
    assert_refused(
        &pool,
        "node_file_has_version",
        "BEGIN; UPDATE node SET current_version = NULL WHERE pk = 4; COMMIT;",
    )
    .await;
    assert_accepted(
        &pool,
        "BEGIN; UPDATE node SET current_version = NULL WHERE pk = 60; \
         DELETE FROM version WHERE file = 60; DELETE FROM node WHERE pk = 60; COMMIT;",
    )
    .await;
    assert_refused(
        &pool,
        "node_folder_has_no_version",
        "BEGIN; UPDATE node SET current_version = 1 WHERE pk = 2; COMMIT;",
    )
    .await;
    // A second root that is a file (in a library without root).
    assert_accepted(&pool, "DELETE FROM node WHERE pk = 5").await;
    assert_refused(
        &pool,
        "node_root_is_folder",
        "INSERT INTO node (organization, library, parent, name, is_file) VALUES (1, 2, NULL, '', true)",
    )
    .await;
    // On insert (no trigger), the CHECK is what refuses a self-parent.
    assert_refused(
        &pool,
        "node_not_own_parent",
        "INSERT INTO node (pk, organization, library, parent, name, is_file) OVERRIDING SYSTEM VALUE \
         VALUES (50, 1, 1, 50, 'self', false)",
    )
    .await;
    let position = "INSERT INTO chunk_in_version (organization, version, chunk, position) VALUES";
    assert_refused(
        &pool,
        "chunk_in_version_version_position_key",
        &format!("{position} (1, 1, 1, 0)"),
    )
    .await;
    assert_refused(
        &pool,
        "chunk_in_version_position_check",
        &format!("{position} (1, 1, 1, -1)"),
    )
    .await;
    assert_refused(
        &pool,
        "chunk_organization_library_fkey",
        "INSERT INTO chunk (organization, library, hash, storage, plain_size) \
         VALUES (1, 3, '\\x000000000000000000000000000000ff', 1, 1)",
    )
    .await;
}

/// The constant columns carry the value a composite FK checks. If one could be
/// set to the other value, its FK would check the wrong thing.
#[sqlx::test]
async fn constant_columns_cannot_be_changed(pool: PgPool) {
    fixture(&pool).await;
    assert_refused(
        &pool,
        "node_parent_is_file_check",
        "UPDATE node SET parent_is_file = true WHERE pk = 2",
    )
    .await;
    assert_refused(
        &pool,
        "version_file_is_file_check",
        "UPDATE version SET file_is_file = false WHERE pk = 1",
    )
    .await;
    assert_refused(
        &pool,
        "chunk_in_version_chunk_ready_check",
        "UPDATE chunk_in_version SET chunk_ready = false",
    )
    .await;
    assert_refused(
        &pool,
        "organization_owner_joined_check",
        "UPDATE organization SET owner_joined = false WHERE pk = 1",
    )
    .await;
}

/// Plain foreign keys: references to rows that don't exist.
#[sqlx::test]
async fn references_must_exist(pool: PgPool) {
    fixture(&pool).await;
    for (constraint, sql) in [
        (
            "identity_account_fkey",
            "INSERT INTO identity (account, kind, details) VALUES (99, 'password', '{\"hash\": \"h\"}')",
        ),
        (
            "user_account_fkey",
            "INSERT INTO \"user\" (account, organization) VALUES (99, 1)",
        ),
        (
            "user_organization_fkey",
            "INSERT INTO \"user\" (account, organization) VALUES (3, 99)",
        ),
        (
            "storage_organization_fkey",
            "INSERT INTO storage (organization, name, implementation, details) VALUES (99, 's', 'filesystem', '{}')",
        ),
        (
            "organization_default_storage_fkey",
            "UPDATE organization SET default_storage = 99 WHERE pk = 1",
        ),
        (
            "library_owner_fkey",
            "INSERT INTO library (name, owner, storage) VALUES ('l', 99, 1)",
        ),
        (
            "library_storage_fkey",
            "INSERT INTO library (name, owner, storage) VALUES ('l', 1, 99)",
        ),
        (
            "namespace_organization_fkey",
            "INSERT INTO namespace (organization, name) VALUES (99, 'n')",
        ),
        (
            "chunk_organization_fkey",
            "INSERT INTO chunk (organization, hash, storage, plain_size) VALUES (99, '\\x000000000000000000000000000000ee', 1, 1)",
        ),
        (
            "chunk_storage_fkey",
            "INSERT INTO chunk (organization, hash, storage, plain_size) VALUES (1, '\\x000000000000000000000000000000ee', 99, 1)",
        ),
    ] {
        assert_refused(&pool, constraint, sql).await;
    }
}
