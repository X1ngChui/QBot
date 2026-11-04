//! The behavior every `MemoryStore` must have. Accounts 1..=40 must exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::EpisodeId;
use crate::memories::{Change, Durability, MemoryState, MemoryStore, Statement, Support};

fn acct(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

fn support(episode: i64, author: AccountId) -> Support {
    Support {
        episode: EpisodeId::new(episode),
        message: MessageId::new(episode * 10).unwrap(),
        author: Some(author),
        quote: format!("quote {episode}"),
        at: at(episode),
    }
}

fn said(subject: Option<AccountId>, text: &str, episode: i64, author: AccountId) -> Statement {
    Statement {
        subject,
        term: None,
        text: text.to_owned(),
        durability: Durability::Lasting,
        support: support(episode, author),
    }
}

pub async fn run(store: &dyn MemoryStore) {
    remembering_and_repeating(store).await;
    replacing_and_ending(store).await;
    changes_name_only_their_own_subject(store).await;
    group_terms(store).await;
    deleting(store).await;
}

async fn remembering_and_repeating(store: &dyn MemoryStore) {
    let g = group(400);
    let p = acct(1);
    let changes = [Change::New(said(Some(p), "lives in Hangzhou", 1, p))];
    let first = store.apply(g, &changes).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
        store.apply(g, &changes).await.unwrap(),
        first,
        "applying an episode again changes nothing"
    );
    let id = first[0];
    // The same statement in other spacing and case, and a repetition by id.
    store
        .apply(
            g,
            &[Change::New(said(Some(p), "Lives  in HANGZHOU", 2, acct(2)))],
        )
        .await
        .unwrap();
    store
        .apply(
            g,
            &[Change::Same {
                id,
                subject: Some(p),
                support: support(3, p),
            }],
        )
        .await
        .unwrap();
    let memory = store.get(g, id).await.unwrap().unwrap();
    assert_eq!(memory.episodes(), 3);
    assert_eq!(memory.own_word(), 2);
    assert_eq!((memory.first, memory.last), (at(1), at(3)));
    assert_eq!(memory.text, "lives in Hangzhou", "the first wording stays");
    assert_eq!(store.current(g, &[Some(p)]).await.unwrap().len(), 1);
}

async fn replacing_and_ending(store: &dyn MemoryStore) {
    let g = group(401);
    let p = acct(3);
    let old = store
        .apply(g, &[Change::New(said(Some(p), "works at a bakery", 1, p))])
        .await
        .unwrap()[0];
    let new = store
        .apply(
            g,
            &[Change::Replace {
                id: old,
                with: said(Some(p), "works at a bookshop", 2, p),
            }],
        )
        .await
        .unwrap()[0];
    assert_ne!(old, new);
    let replaced = store.get(g, old).await.unwrap().unwrap();
    assert_eq!(
        replaced.state,
        MemoryState::Replaced {
            by: Some(new),
            at: at(2)
        },
        "the old memory is kept as replaced"
    );
    let current = store.current(g, &[Some(p)]).await.unwrap();
    assert_eq!(current.iter().map(|m| m.id).collect::<Vec<_>>(), [new]);
    // Replacing again (the episode applied twice) changes nothing.
    store
        .apply(
            g,
            &[Change::Replace {
                id: old,
                with: said(Some(p), "works at a bookshop", 2, p),
            }],
        )
        .await
        .unwrap();
    assert_eq!(store.about(g, Some(p)).await.unwrap().len(), 2);

    store
        .apply(
            g,
            &[Change::End {
                id: new,
                subject: Some(p),
                at: at(3),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(g, new).await.unwrap().unwrap().state,
        MemoryState::Ended { at: at(3) }
    );
    assert!(store.current(g, &[Some(p)]).await.unwrap().is_empty());
    // Said again after it ended: remembered anew.
    let again = store
        .apply(
            g,
            &[Change::New(said(Some(p), "works at a bookshop", 4, p))],
        )
        .await
        .unwrap()[0];
    assert_ne!(again, new);
    // Deleting the replacement leaves the replaced memory replaced, by nothing.
    assert!(store.delete(g, new).await.unwrap());
    assert_eq!(
        store.get(g, old).await.unwrap().unwrap().state,
        MemoryState::Replaced {
            by: None,
            at: at(2)
        }
    );
}

async fn changes_name_only_their_own_subject(store: &dyn MemoryStore) {
    let (g, h) = (group(402), group(403));
    let (p, q) = (acct(4), acct(5));
    let id = store
        .apply(g, &[Change::New(said(Some(p), "has a cat", 1, p))])
        .await
        .unwrap()[0];
    // Another subject, or another group, cannot touch it.
    let touched = store
        .apply(
            g,
            &[
                Change::Same {
                    id,
                    subject: Some(q),
                    support: support(2, q),
                },
                Change::End {
                    id,
                    subject: Some(q),
                    at: at(2),
                },
            ],
        )
        .await
        .unwrap();
    assert!(touched.is_empty());
    store
        .apply(
            h,
            &[Change::End {
                id,
                subject: Some(p),
                at: at(2),
            }],
        )
        .await
        .unwrap();
    let memory = store.get(g, id).await.unwrap().unwrap();
    assert!(memory.is_current());
    assert_eq!(memory.episodes(), 1);
    assert!(store.get(h, id).await.unwrap().is_none());
    // A replacement naming someone else's memory is remembered as new.
    let new = store
        .apply(
            g,
            &[Change::Replace {
                id,
                with: said(Some(q), "has a dog", 3, q),
            }],
        )
        .await
        .unwrap()[0];
    assert!(store.get(g, id).await.unwrap().unwrap().is_current());
    assert_eq!(store.get(g, new).await.unwrap().unwrap().subject, Some(q));
}

async fn group_terms(store: &dyn MemoryStore) {
    let g = group(404);
    let p = acct(6);
    let term = |text: &str, episode| Statement {
        subject: None,
        term: Some("ZZ".to_owned()),
        text: text.to_owned(),
        durability: Durability::Ongoing,
        support: support(episode, p),
    };
    let a = store
        .apply(g, &[Change::New(term("the weekly raid", 1))])
        .await
        .unwrap()[0];
    let b = store
        .apply(g, &[Change::New(term("the night shift", 2))])
        .await
        .unwrap()[0];
    assert_ne!(a, b, "two meanings of one term are two memories");
    let current = store.current(g, &[None]).await.unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].display(), "ZZ: the weekly raid");
    assert!(store.current(g, &[Some(p)]).await.unwrap().is_empty());
}

async fn deleting(store: &dyn MemoryStore) {
    let (g, h) = (group(405), group(406));
    let (p, q) = (acct(7), acct(8));
    let mine = store
        .apply(
            g,
            &[
                Change::New(said(Some(p), "studies law", 1, p)),
                Change::New(said(Some(p), "plays chess", 1, p)),
                Change::New(said(Some(q), "plays go", 1, q)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(mine.len(), 3);
    store
        .apply(h, &[Change::New(said(Some(p), "studies law", 1, p))])
        .await
        .unwrap();
    assert!(
        !store.delete(h, mine[0]).await.unwrap(),
        "not in that group"
    );
    assert!(store.delete(g, mine[0]).await.unwrap());
    assert!(store.get(g, mine[0]).await.unwrap().is_none());
    assert_eq!(store.delete_about(g, p).await.unwrap(), 1);
    assert!(store.about(g, Some(p)).await.unwrap().is_empty());
    assert_eq!(store.about(g, Some(q)).await.unwrap().len(), 1);
    assert_eq!(
        store.about(h, Some(p)).await.unwrap().len(),
        1,
        "other groups keep theirs"
    );
    let left = store.about(g, Some(q)).await.unwrap()[0].id;
    assert_eq!(store.evict(g, &[left, left]).await.unwrap(), 1);
}
