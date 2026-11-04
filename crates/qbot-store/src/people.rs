//! Who is who in a group, as one read model for every run's cached prefix: which member numbers
//! are one person, who is blocked, what each member is and was shown as (from the display names
//! their archived lines were sent under) and the names people use for them. Every record is the
//! group's own: links made in other groups and anything stored there play no part.
//!
//! Not every member is listed: a group can have thousands. A person is listed when one of their
//! accounts spoke recently, is linked or blocked, or has a name a person set; the most recently
//! active first, up to a bound. Memories and notes are not here: a run is shown those for the
//! people in its chat.

use std::collections::{BTreeMap, HashMap, HashSet};

use qbot_core::{AccountId, GroupId, MemberNo, UnixMillis};
use qbot_memory::names::KnownName;
use sqlx::Row;

use crate::archive::PgArchive;
use crate::error::StoreError;
use crate::names::known_names;

/// Which members the roster lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterSettings {
    /// A member who spoke within this long is listed.
    pub recent_ms: i64,
    /// People listed at most, the most recently active kept.
    pub max_people: usize,
}

/// The people of a group the roster lists, by their lowest member number, and every name the
/// group uses, for telling whether one is shared.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupPeople {
    pub people: Vec<Person>,
    /// Every name of the group, listed people or not, with the member each belongs to.
    pub names: Vec<(MemberNo, KnownName)>,
}

/// One person: one or more numbered accounts of the group (two or more are linked accounts).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Person {
    /// Their numbered accounts here, by number. The account is for asking the platform about
    /// them; it is never shown.
    pub members: Vec<(MemberNo, AccountId)>,
    /// Which of `members` are blocked now, ascending.
    pub blocked: Vec<MemberNo>,
    /// Display names their lines were sent under, the most recent first, per account.
    pub shown: Vec<ShownName>,
    /// The names people use for them, the most used first.
    pub names: Vec<KnownName>,
    /// Their latest line.
    pub active: Option<UnixMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownName {
    pub member: MemberNo,
    pub text: String,
    /// The latest line sent under it.
    pub last: UnixMillis,
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::corrupt(error.to_string())
}

fn member_no(n: i32) -> Result<MemberNo, StoreError> {
    u32::try_from(n).map(MemberNo::new).map_err(corrupt)
}

/// Display names kept per account: the current one and a few before it.
const SHOWN_PER_ACCOUNT: usize = 4;

impl PgArchive {
    /// The group's roster as of `now` (a block that has run out is not in force), with the people
    /// of `include` listed whatever their activity. Activity and display names count lines up to
    /// the start of `now`'s UTC day, so the roster changes by activity at most once a day and
    /// stays in the cached prompt prefix in between.
    pub async fn people(
        &self,
        group: GroupId,
        now: UnixMillis,
        settings: RosterSettings,
        include: &[AccountId],
    ) -> Result<GroupPeople, StoreError> {
        let (pool, g) = (self.pool(), group.get());
        let as_of = qbot_memory::worth::as_of_day(now).get();
        // Listed whatever their activity: those asked for, and those with a record that matters
        // for how the group is answered (a link, a block, a name a person set).
        let mut forced: HashSet<i64> = include.iter().map(|a| a.get()).collect();
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
        let mut wanted: HashSet<i64> = HashSet::new();
        for row in
            sqlx::query("SELECT low_account, high_account FROM account_link WHERE group_id = $1")
                .bind(g)
                .fetch_all(pool)
                .await?
        {
            let (low, high): (i64, i64) = (row.get("low_account"), row.get("high_account"));
            forced.extend([low, high]);
            let (a, b) = (find(&mut person, low), find(&mut person, high));
            person.insert(a.max(b), a.min(b));
        }
        let mut people: BTreeMap<i64, Person> = BTreeMap::new();
        let mut root_of: HashMap<i64, i64> = HashMap::new();
        for (account, number) in &numbers {
            let root = find(&mut person, *account);
            root_of.insert(*account, root);
            people
                .entry(root)
                .or_default()
                .members
                .push((*number, AccountId::new(*account).map_err(corrupt)?));
        }

        let blocked: Vec<i64> = sqlx::query_scalar(
            "SELECT account_id FROM group_block \
             WHERE group_id = $1 AND (until_ms IS NULL OR until_ms > $2)",
        )
        .bind(g)
        .bind(now.get())
        .fetch_all(pool)
        .await?;
        for account in blocked {
            if let (Some(number), Some(root)) = (numbers.get(&account), root_of.get(&account)) {
                people.entry(*root).or_default().blocked.push(*number);
                forced.insert(account);
            }
        }

        for row in sqlx::query(
            "SELECT account_id, max(at_ms) AS last FROM chat_line \
             WHERE group_id = $1 AND speaker = 'member' AND at_ms > $2 AND at_ms <= $3 \
             GROUP BY account_id",
        )
        .bind(g)
        .bind(as_of - settings.recent_ms)
        .bind(as_of)
        .fetch_all(pool)
        .await?
        {
            let account: i64 = row.get("account_id");
            if let Some(root) = root_of.get(&account) {
                let last = UnixMillis::new(row.get("last"));
                let p = people.entry(*root).or_default();
                p.active = Some(p.active.map_or(last, |a| a.max(last)));
                wanted.insert(account);
            }
        }

        let all_names = known_names(pool, group, None).await.map_err(corrupt)?;
        let mut names = Vec::new();
        for name in all_names {
            let account = name.account.get();
            if name.set {
                forced.insert(account);
            }
            if let Some(number) = numbers.get(&account) {
                names.push((*number, name));
            }
        }

        // The people listed: those with a record or asked for, and of the rest, those who spoke
        // recently, the most recently active first, up to the bound.
        let has =
            |p: &Person, set: &HashSet<i64>| p.members.iter().any(|(_, a)| set.contains(&a.get()));
        let mut listed: Vec<i64> = people
            .iter()
            .filter(|(_, p)| has(p, &wanted) && !has(p, &forced))
            .map(|(root, _)| *root)
            .collect();
        listed.sort_by_key(|root| std::cmp::Reverse(people[root].active));
        listed.truncate(settings.max_people);
        let mut listed: HashSet<i64> = listed.into_iter().collect();
        listed.extend(
            people
                .iter()
                .filter(|(_, p)| has(p, &forced))
                .map(|(root, _)| *root),
        );
        people.retain(|root, _| listed.contains(root));
        let accounts: Vec<i64> = people
            .values()
            .flat_map(|p| p.members.iter().map(|(_, a)| a.get()))
            .collect();

        for row in sqlx::query(
            "SELECT account_id, sender_name, max(at_ms) AS last FROM chat_line \
             WHERE group_id = $1 AND account_id = ANY($2) AND sender_name IS NOT NULL \
             AND at_ms <= $3 GROUP BY account_id, sender_name",
        )
        .bind(g)
        .bind(&accounts)
        .bind(as_of)
        .fetch_all(pool)
        .await?
        {
            let account: i64 = row.get("account_id");
            if let (Some(number), Some(p)) = (
                numbers.get(&account),
                root_of.get(&account).and_then(|r| people.get_mut(r)),
            ) {
                p.shown.push(ShownName {
                    member: *number,
                    text: row.get("sender_name"),
                    last: UnixMillis::new(row.get("last")),
                });
            }
        }
        for (_, name) in &names {
            if let Some(p) = root_of
                .get(&name.account.get())
                .and_then(|r| people.get_mut(r))
            {
                p.names.push(name.clone());
            }
        }

        let mut out: Vec<Person> = people
            .into_values()
            .map(|mut p| {
                p.members.sort();
                p.blocked.sort();
                p.shown
                    .sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.text.cmp(&b.text)));
                let mut per_member: HashMap<MemberNo, usize> = HashMap::new();
                p.shown.retain(|s| {
                    let n = per_member.entry(s.member).or_default();
                    *n += 1;
                    *n <= SHOWN_PER_ACCOUNT
                });
                p.names.sort_by(|a, b| {
                    b.set
                        .cmp(&a.set)
                        .then(b.uses().cmp(&a.uses()))
                        .then(b.last.cmp(&a.last))
                        .then_with(|| a.key.cmp(&b.key))
                });
                p
            })
            .collect();
        out.sort_by_key(|p| p.members.first().map(|(n, _)| *n));
        Ok(GroupPeople { people: out, names })
    }
}
