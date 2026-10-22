//! What the bot keeps about the people of a group, as one read model: who is blocked, which
//! member numbers are one person, the names they go by, the notes members wrote about them and
//! the facts learned from chat. Every record is the group's own: links made in other groups and
//! anything stored there play no part. Every run shows it, so it is read whole and ordered
//! deterministically: it changes only when the stored records do, or when the confidence of a
//! belief, computed as of the start of the day, moves.

use std::collections::{BTreeMap, HashMap};

use qbot_core::{AccountId, GroupId, MemberNo, UnixMillis};
use qbot_memory::belief::{Activity, as_of_day, persistence};
use qbot_memory::facts::{DecayPolicy, Fact};
use qbot_memory::predicates::DecayClass;
use sqlx::Row;

use crate::archive::PgArchive;
use crate::error::StoreError;

/// The people of a group the bot keeps anything about, by their lowest member number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupPeople {
    pub people: Vec<Person>,
}

/// One person: one or more numbered accounts of the group (two or more are linked accounts).
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Facts learned from chat, the most worth showing first.
    pub facts: Vec<KnownFact>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KnownName {
    pub text: String,
    /// Vouched for (by the person or an owner); otherwise a lead from chat.
    pub confirmed: bool,
    /// The probability that it is their name (`qbot_memory::belief`): 1 when confirmed.
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownNote {
    pub text: String,
    pub updated: UnixMillis,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KnownFact {
    pub predicate: String,
    pub value: String,
    /// Distinct episodes that supported it.
    pub supports: u32,
    pub last_confirmed: UnixMillis,
    /// The probability that it holds (`qbot_memory::belief`), weighed against the competing
    /// claims about the same account.
    pub confidence: f64,
    /// What it is worth where room is limited: confidence times how current it still is in the
    /// group's conversation (`DecayPolicy::value`). Orders the facts; not shown.
    pub worth: f64,
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
    /// is not in force), with the confidence of beliefs as of the start of `now`'s day under
    /// `decay`. Records about an account with no member number here are not shown.
    pub async fn people(
        &self,
        group: GroupId,
        now: UnixMillis,
        decay: &DecayPolicy,
    ) -> Result<GroupPeople, StoreError> {
        let as_of = as_of_day(now);
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
            "SELECT text, status = 'confirmed' AS confirmed, confidence, last_evidence_ms, account_id \
             FROM alias WHERE group_id = $1 AND NOT removed AND status IN ('candidate', 'confirmed')",
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
            let credibility = f64::from(row.get::<f32, _>("confidence"));
            let confidence = if confirmed {
                credibility
            } else {
                credibility
                    * persistence(
                        decay.half_life(DecayClass::Stable),
                        UnixMillis::new(row.get("last_evidence_ms")),
                        as_of,
                    )
            };
            match person.names.iter_mut().find(|n| n.text == text) {
                Some(known) => {
                    known.confirmed |= confirmed;
                    known.confidence = known.confidence.max(confidence);
                }
                None => person.names.push(KnownName {
                    text,
                    confirmed,
                    confidence,
                }),
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

        // Facts weigh against the competing claims about the same account; how current they are
        // follows the group's own episodes.
        let mut episodes = Vec::new();
        for row in sqlx::query(
            "SELECT ended_ms, participants FROM episode WHERE group_id = $1 ORDER BY ended_ms",
        )
        .bind(g)
        .fetch_all(pool)
        .await?
        {
            let people = row
                .get::<Vec<i64>, _>("participants")
                .into_iter()
                .map(AccountId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(corrupt)?;
            episodes.push((UnixMillis::new(row.get("ended_ms")), people));
        }
        let activity = Activity::new(episodes);
        let mut about: BTreeMap<i64, Vec<Fact>> = BTreeMap::new();
        for row in sqlx::query(&format!(
            "SELECT {} FROM fact WHERE group_id = $1 AND status = 'active' \
             AND subject_account IS NOT NULL ORDER BY fact_id",
            crate::facts::COLS
        ))
        .bind(g)
        .fetch_all(pool)
        .await?
        {
            let fact = crate::facts::to_fact(&row).map_err(corrupt)?;
            if let Some(subject) = fact.subject {
                about.entry(subject.get()).or_default().push(fact);
            }
        }
        for (account, facts) in about {
            let Some(person) = person_of(account).and_then(|h| people.get_mut(&h)) else {
                continue;
            };
            let assessed = decay.assess(&facts, &activity, as_of);
            for (fact, (confidence, worth)) in facts.into_iter().zip(assessed) {
                person.facts.push(KnownFact {
                    predicate: fact.predicate,
                    value: fact.label.unwrap_or(fact.object),
                    supports: fact.supports,
                    last_confirmed: fact.last_confirmed,
                    confidence,
                    worth,
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
                p.facts.sort_by(|a, b| {
                    b.worth
                        .total_cmp(&a.worth)
                        .then_with(|| a.predicate.cmp(&b.predicate))
                        .then(b.last_confirmed.cmp(&a.last_confirmed))
                        .then_with(|| a.value.cmp(&b.value))
                });
                p
            })
            .collect();
        out.sort_by_key(|p| p.members.first().map(|(n, _)| *n));
        Ok(GroupPeople { people: out })
    }
}
