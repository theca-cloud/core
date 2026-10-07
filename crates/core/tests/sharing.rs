//! What an organization's owner manages: its libraries' members and its dedup
//! namespaces. Account 1 (a@x.ch) owns organization one, where account 2
//! (b@x.ch) is a member; account 3 (c@x.ch) owns organization two.

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

fn code(failure: Failure) -> (StatusCode, String) {
    (
        failure.0,
        failure.1["code"].as_str().unwrap_or_default().to_owned(),
    )
}

async fn libraries(client: &Client) -> Vec<api::Library> {
    client.get("/api/v1/libraries").await.unwrap()
}

#[sqlx::test]
async fn library_members(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let carol = w.as_user(3).await;
    let (one, bob_user, carol_user) = (w.organizations[0], w.users[1], w.users[2]);

    // The owner sees every library of the organization, member of it or not.
    let docs = alice.create_library("Docs").await;
    let private = bob.create_library("Bob's").await;
    let all: Vec<api::OrganizationLibrary> = alice
        .get(&format!("/api/v1/organizations/{one}/libraries"))
        .await
        .unwrap();
    let all: Vec<(&str, i64)> = all.iter().map(|l| (l.name.as_str(), l.members)).collect();
    assert_eq!(all, [("Bob's", 1), ("Docs", 1)]);
    let refused = bob
        .get::<Value>(&format!("/api/v1/organizations/{one}/libraries"))
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::NOT_FOUND);

    // Added read-only, then writing; members see who else is in.
    let member = format!("/api/v1/libraries/{}/members/{bob_user}", docs.id);
    let members: Vec<api::LibraryMember> = alice
        .json("PUT", &member, &json!({ "role": "read" }))
        .await
        .unwrap();
    assert_eq!(members.len(), 2);
    let docs_for_bob = |list: Vec<api::Library>| list.into_iter().find(|l| l.id == docs.id);
    assert!(!docs_for_bob(libraries(&bob).await).unwrap().writable);
    alice
        .json::<Value>("PUT", &member, &json!({ "role": "write" }))
        .await
        .unwrap();
    assert!(docs_for_bob(libraries(&bob).await).unwrap().writable);
    let seen: Vec<api::LibraryMember> = bob
        .get(&format!("/api/v1/libraries/{}/members", docs.id))
        .await
        .unwrap();
    let seen: Vec<(&str, api::Role)> = seen.iter().map(|m| (m.email.as_str(), m.role)).collect();
    assert_eq!(
        seen,
        [("a@x.ch", api::Role::Write), ("b@x.ch", api::Role::Write)]
    );

    // Only the owner manages; only members of the organization are added.
    let refused = bob
        .json::<Value>(
            "PUT",
            &format!("/api/v1/libraries/{}/members/{bob_user}", private.id),
            &json!({ "role": "read" }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::NOT_FOUND);
    let outsider = alice
        .json::<Value>(
            "PUT",
            &format!("/api/v1/libraries/{}/members/{carol_user}", docs.id),
            &json!({ "role": "read" }),
        )
        .await
        .unwrap_err();
    assert_eq!(outsider.0, StatusCode::NOT_FOUND);
    let role = alice
        .json::<Value>("PUT", &member, &json!({ "role": "admin" }))
        .await
        .unwrap_err();
    assert_eq!(code(role), (StatusCode::BAD_REQUEST, "invalid_role".into()));
    let hidden = carol
        .get::<Value>(&format!("/api/v1/libraries/{}/members", docs.id))
        .await
        .unwrap_err();
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);

    // The owner manages a library it isn't a member of.
    let in_private = format!("/api/v1/libraries/{}/members", private.id);
    assert_eq!(
        alice
            .get::<Vec<api::LibraryMember>>(&in_private)
            .await
            .unwrap()
            .len(),
        1
    );

    // An invited member (not joined yet) can be added already.
    w.sql(r#"INSERT INTO "user" (pk, account, organization) OVERRIDING SYSTEM VALUE VALUES (4, 3, 1)"#)
        .await;
    let invited: Uuid = sqlx::query_scalar(r#"SELECT uuid FROM "user" WHERE pk = 4"#)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let members: Vec<api::LibraryMember> = alice
        .json(
            "PUT",
            &format!("/api/v1/libraries/{}/members/{invited}", docs.id),
            &json!({ "role": "read" }),
        )
        .await
        .unwrap();
    assert!(members.iter().any(|m| m.user == invited && !m.joined));

    // Removed: the access goes at once.
    assert_eq!(
        alice.call("DELETE", &member, None, true).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(docs_for_bob(libraries(&bob).await).is_none());
    assert_eq!(
        alice.call("DELETE", &member, None, true).await.0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn namespaces(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let carol = w.as_user(3).await;
    let one = w.organizations[0];
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    let theirs = carol.create_library("Theirs").await;

    let list = format!("/api/v1/organizations/{one}/namespaces");
    let team: api::Namespace = alice
        .json("POST", &list, &json!({ "name": "Team" }))
        .await
        .unwrap();
    assert!(team.libraries.is_empty());
    let taken = alice
        .json::<Value>("POST", &list, &json!({ "name": "team" }))
        .await
        .unwrap_err();
    assert_eq!(
        code(taken),
        (StatusCode::CONFLICT, "namespace_name_taken".into())
    );

    // Libraries in and out, only the organization's own.
    let ns = format!("/api/v1/namespaces/{}", team.id);
    for library in [docs.id, photos.id, docs.id] {
        alice
            .json::<Value>("PUT", &format!("{ns}/libraries/{library}"), &json!({}))
            .await
            .unwrap();
    }
    let elsewhere = alice
        .json::<Value>("PUT", &format!("{ns}/libraries/{}", theirs.id), &json!({}))
        .await
        .unwrap_err();
    assert_eq!(elsewhere.0, StatusCode::NOT_FOUND);
    let listed: Vec<api::Namespace> = alice.get(&list).await.unwrap();
    assert_eq!(listed[0].libraries, [docs.id, photos.id]);
    let out: api::Namespace = alice
        .json(
            "DELETE",
            &format!("{ns}/libraries/{}", photos.id),
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(out.libraries, [docs.id]);

    // Renamed, then deleted; the owner's only.
    let renamed: api::Namespace = alice
        .json("PATCH", &ns, &json!({ "name": "Staff" }))
        .await
        .unwrap();
    assert_eq!(renamed.name, "Staff");
    assert_eq!(
        bob.get::<Value>(&list).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    let refused = bob
        .json::<Value>("PATCH", &ns, &json!({ "name": "Mine" }))
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::NOT_FOUND);

    // A library created in a namespace: the owner only (a member could
    // otherwise probe the namespace's libraries).
    let create = |name: &str| json!({ "name": name, "organization": one, "namespaces": [team.id] });
    let probe = bob
        .json::<Value>("POST", "/api/v1/libraries", &create("Probe"))
        .await
        .unwrap_err();
    assert_eq!(
        code(probe),
        (StatusCode::FORBIDDEN, "owner_required".into())
    );
    alice
        .json::<api::Library>("POST", "/api/v1/libraries", &create("Shared"))
        .await
        .unwrap();

    assert_eq!(
        alice.call("DELETE", &ns, None, true).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(
        alice
            .get::<Vec<api::Namespace>>(&list)
            .await
            .unwrap()
            .is_empty()
    );
}
