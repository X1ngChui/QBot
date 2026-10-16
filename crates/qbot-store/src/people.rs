//! What the bot keeps about the people of a group, as one read model: who is blocked, which
//! member numbers are one person, the names they go by, the notes members wrote about them and
//! the facts learned from chat. Every record is the group's own: links made in other groups and
//! anything stored there play no part. Every run shows it, so it is read whole and ordered
//! deterministically: it changes only when the stored records do.

use std::collections::{BTreeMap, HashMap};

use qbot_core::{AccountId, GroupId, MemberNo, UnixMillis};
use sqlx::Row;

use crate::archive::PgArchive;
use crate::error::StoreError;

/// The people of a group the bot keeps anything about, by their lowest member number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupPeople {
    pub people: Vec<Person>,
}

/// One person: one or more numbered accounts of the group (two or more are linked accounts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    /// Their numbered accounts here, by number. The account is for asking the platform about
    /// them; it is never shown.
    pub members: Vec<(MemberNo, AccountId)>,
    /// Which of `members` are blocked now, ascending.
    pub blocked: Vec<MemberNo>,
    /// Names they go by besides their display name, by text.
    pub names: Vec<KnownName>,
    /// Notes members wrote about them, oldest first.
    pub notes: Vec<KnownNote>,
    /// Facts learned from chat, by predicate, then the most recently confirmed first.
    pub facts: Vec<KnownFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownName {
    pub text: String,
    /// Vouched for (by the person or an owner); otherwise a lead from chat.
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownNote {
    pub text: String,
    pub updated: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownFact {
    pub predicate: String,
    pub value: String,
    /// Distinct episodes that supported it.
    pub supports: u32,
    pub last_confirmed: UnixMillis,
}

impl Person {
    fn has_records(&self) -> bool {
        self.members.len() > 1
            || !self.blocked.is_empty()
            || !self.names.is_empty()
            || !self.notes.is_empty()
            || !self.facts.is_empty()
    }
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::corrupt(error.to_string())
}

fn member_no(n: i32) -> Result<MemberNo, StoreError> {
    u32::try_from(n).map(MemberNo::new).map_err(corrupt)
}

impl PgArchive {
    /// Everyone in `group` the bot keeps a record about, as of `now` (a block that has run out
    /// is not in force). Records about an account with no member number here are not shown.
    pub async fn people(&self, group: GroupId, now: UnixMillis) -> Result<GroupPeople, StoreError> {
        let (pool, g) = (self.pool(), group.get());
        let mut numbers: HashMap<i64, MemberNo> = HashMap::new();
        for row in sqlx::query("SELECT number, account_id FROM member_number WHERE group_id = $1")
            .bind(g)
            .fetch_all(pool)
            .await?
        {
            numbers.insert(row.get("account_id"), member_no(row.get("number"))?);
        }
        // One person per set of accounts linked to each other in this group, named by its
        // lowest account.
        let mut person: BTreeMap<i64, i64> = numbers.keys().map(|a| (*a, *a)).collect();
        fn find(person: &mut BTreeMap<i64, i64>, account: i64) -> i64 {
            let parent = *person.entry(account).or_insert(account);
            if parent == account {
                return account;
            }
            let root = find(person, parent);
            person.insert(account, root);
            root
        }
        for row in
            sqlx::query("SELECT low_account, high_account FROM account_link WHERE group_id = $1")
                .bind(g)
                .fetch_all(pool)
                .await?
        {
            let (a, b) = (
                find(&mut person, row.get("low_account")),
                find(&mut person, row.get("high_account")),
            );
            person.insert(a.max(b), a.min(b));
        }
        let mut people: BTreeMap<i64, Person> = BTreeMap::new();
        let mut numbered: HashMap<i64, (MemberNo, i64)> = HashMap::new();
        for (account, number) in &numbers {
            let root = find(&mut person, *account);
            numbered.insert(*account, (*number, root));
            people
                .entry(root)
                .or_default()
                .members
                .push((*number, AccountId::new(*account).map_err(corrupt)?));
        }
        let person_of = |account: i64| numbered.get(&account).map(|(_, h)| *h);

        let blocked: Vec<i64> = sqlx::query_scalar(
            "SELECT account_id FROM group_block \
             WHERE group_id = $1 AND (until_ms IS NULL OR until_ms > $2)",
        )
        .bind(g)
        .bind(now.get())
        .fetch_all(pool)
        .await?;
        for account in blocked {
            if let Some((number, h)) = numbered.get(&account) {
                people.entry(*h).or_default().blocked.push(*number);
            }
        }

        // Names in force.
        let names = sqlx::query(
            "SELECT text, status = 'confirmed' AS confirmed, account_id FROM alias \
             WHERE group_id = $1 AND NOT removed AND status IN ('candidate', 'confirmed')",
        )
        .bind(g)
        .fetch_all(pool)
        .await?;
        for row in names {
            let Some(person) = person_of(row.get("account_id")).and_then(|h| people.get_mut(&h))
            else {
                continue;
            };
            let (text, confirmed): (String, bool) = (row.get("text"), row.get("confirmed"));
            match person.names.iter_mut().find(|n| n.text == text) {
                Some(known) => known.confirmed |= confirmed,
                None => person.names.push(KnownName { text, confirmed }),
            }
        }

        for row in sqlx::query(
            "SELECT account_id, text, updated_ms FROM note WHERE group_id = $1 ORDER BY note_id",
        )
        .bind(g)
        .fetch_all(pool)
        .await?
        {
            if let Some(person) = person_of(row.get("account_id")).and_then(|h| people.get_mut(&h))
            {
                person.notes.push(KnownNote {
                    text: row.get("text"),
                    updated: UnixMillis::new(row.get("updated_ms")),
                });
            }
        }

        for row in sqlx::query(
            "SELECT subject_account, predicate, coalesce(label, object) AS value, supports, \
                    last_confirmed_ms \
             FROM fact WHERE group_id = $1 AND status = 'active' AND subject_account IS NOT NULL \
             ORDER BY predicate COLLATE \"C\", last_confirmed_ms DESC, fact_id DESC",
        )
        .bind(g)
        .fetch_all(pool)
        .await?
        {
            if let Some(person) =
                person_of(row.get("subject_account")).and_then(|h| people.get_mut(&h))
            {
                person.facts.push(KnownFact {
                    predicate: row.get("predicate"),
                    value: row.get("value"),
                    supports: u32::try_from(row.get::<i32, _>("supports")).map_err(corrupt)?,
                    last_confirmed: UnixMillis::new(row.get("last_confirmed_ms")),
                });
            }
        }

        let mut out: Vec<Person> = people
            .into_values()
            .filter(Person::has_records)
            .map(|mut p| {
                p.members.sort();
                p.blocked.sort();
                p.names.sort_by(|a, b| a.text.cmp(&b.text));
                p
            })
            .collect();
        out.sort_by_key(|p| p.members.first().map(|(n, _)| *n));
        Ok(GroupPeople { people: out })
    }
}
