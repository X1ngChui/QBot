//! The behavior every `NoteStore` must have; the in-memory store defines it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, UnixMillis};

use crate::notes::{NoteId, NoteStore};

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

/// Run against a store whose accounts `accounts[0..3]` exist.
pub async fn run(store: &dyn NoteStore, accounts: [AccountId; 3]) {
    let (g, h) = (GroupId::new(9301).unwrap(), GroupId::new(9302).unwrap());
    let [a, b, owner] = accounts;
    assert!(store.notes(g, &[a]).await.unwrap().is_empty());

    let first = store
        .add(g, a, "likes early meetings", a, at(1))
        .await
        .unwrap();
    let second = store
        .add(g, a, "owns the group's printer", owner, at(2))
        .await
        .unwrap();
    store
        .add(g, b, "new this month", owner, at(3))
        .await
        .unwrap();
    store.add(h, a, "elsewhere", a, at(4)).await.unwrap();

    let texts =
        |notes: Vec<crate::notes::Note>| notes.into_iter().map(|n| n.text).collect::<Vec<_>>();
    assert_eq!(
        texts(store.notes(g, &[a]).await.unwrap()),
        ["likes early meetings", "owns the group's printer"],
        "oldest first, this group only"
    );
    assert_eq!(
        texts(store.notes(g, &[b, a]).await.unwrap()),
        [
            "new this month",
            "likes early meetings",
            "owns the group's printer"
        ],
        "accounts in the order asked"
    );
    let stored = &store.notes(g, &[a]).await.unwrap()[1];
    assert_eq!(
        (stored.author, stored.created, stored.updated),
        (owner, at(2), at(2))
    );

    assert!(
        store
            .edit(g, second.id, "runs the printer", a, at(5))
            .await
            .unwrap()
    );
    let edited = &store.notes(g, &[a]).await.unwrap()[1];
    assert_eq!(
        (
            edited.text.as_str(),
            edited.author,
            edited.created,
            edited.updated
        ),
        ("runs the printer", a, at(2), at(5))
    );
    assert!(
        !store.edit(h, second.id, "x", a, at(6)).await.unwrap(),
        "another group's id"
    );
    assert!(
        !store
            .edit(g, NoteId::new(999_999), "x", a, at(6))
            .await
            .unwrap()
    );

    assert!(
        !store.remove(h, first.id).await.unwrap(),
        "another group's id"
    );
    assert!(store.remove(g, first.id).await.unwrap());
    assert!(!store.remove(g, first.id).await.unwrap(), "already gone");
    assert_eq!(
        texts(store.notes(g, &[a]).await.unwrap()),
        ["runs the printer"]
    );

    assert_eq!(store.clear(g, &[a, b]).await.unwrap(), 2);
    assert!(store.notes(g, &[a, b]).await.unwrap().is_empty());
    assert_eq!(
        store.notes(h, &[a]).await.unwrap().len(),
        1,
        "other groups keep theirs"
    );
}
