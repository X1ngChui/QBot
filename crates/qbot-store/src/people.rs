//! What the bot keeps about the people of a group, as one read model: who is blocked, which
//! member numbers are one person, the names they go by, the notes members wrote about them and
//! the facts learned from chat. Every run shows it, so it is read whole and ordered
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
        // Each numbered account's number and current (unmerged) holder: the person.
        let mut numbered: HashMap<i64, (MemberNo, i64)> = HashMap::new();
        for row in sqlx::query(
            "SELECT m.number, m.account_id, a.holder_id FROM member_number m \
             JOIN account a ON a.account_id = m.account_id WHERE m.group_id = $1",
        )
        .bind(g)
        .fetch_all(pool)
        .await?
        {
            numbered.insert(
                row.get("account_id"),
                (member_no(row.get("number"))?, row.get("holder_id")),
            );
        }
        let mut people: BTreeMap<i64, Person> = BTreeMap::new();
        for (account, (number, holder)) in &numbered {
            let account = AccountId::new(*account).map_err(corrupt)?;
            people
                .entry(*holder)
                .or_default()
                .members
                .push((*number, account));
        }
        let holder = |account: i64| numbered.get(&account).map(|(_, h)| *h);

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

        // Names in force, of an account or of a person (a holder merged into another one counts
        // as the one it was merged into).
        let names = sqlx::query(
            "WITH RECURSIVE up(start, id, next) AS ( \
                 SELECT holder_id, holder_id, merged_into FROM holder WHERE holder_id IN \
                     (SELECT target_holder FROM alias WHERE group_id = $1 AND target_holder IS NOT NULL) \
               UNION ALL \
                 SELECT up.start, h.holder_id, h.merged_into FROM up JOIN holder h ON h.holder_id = up.next) \
             SELECT a.text, a.status = 'confirmed' AS confirmed, a.target_account, r.id AS root \
             FROM alias a LEFT JOIN up r ON r.start = a.target_holder AND r.next IS NULL \
             WHERE a.group_id = $1 AND NOT a.removed AND a.status IN ('candidate', 'confirmed')",
        )
        .bind(g)
        .fetch_all(pool)
        .await?;
        for row in names {
            let target = match row.get::<Option<i64>, _>("target_account") {
                Some(account) => holder(account),
                None => row.get::<Option<i64>, _>("root"),
            };
            let Some(person) = target.and_then(|h| people.get_mut(&h)) else {
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
            if let Some(person) = holder(row.get("account_id")).and_then(|h| people.get_mut(&h)) {
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
                holder(row.get("subject_account")).and_then(|h| people.get_mut(&h))
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
