//! How what is kept about people reads to a model: the roster (who is shown as what, the names
//! people use for whom), memories with their provenance, notes. Shared by the reply prompt, the
//! extractor's background and `about_member`, so all three say the same thing the same way.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::marker::escape_markers;
use qbot_core::{AccountId, MemberNo, UnixMillis};
use qbot_memory::memories::Memory;
use qbot_memory::names::{CORROBORATED_USES, KnownName, name_key};
use qbot_memory::worth::HalfLives;
use qbot_store::{GroupPeople, Person};
use qbot_wording::{Text, say};

/// How much memory a run is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryView {
    /// Group memories in every run's cached prefix, the most worth first.
    pub group_memories: usize,
    /// Memories per member in a run's opening note, the most worth first.
    pub person_memories: usize,
    pub half_lives: HalfLives,
}

/// Member-written text as it may stand in a prompt: marker-escaped, on one line.
pub(crate) fn line(text: &str) -> String {
    escape_markers(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub(crate) fn date(zone: &TimeZone, at: UnixMillis) -> String {
    Timestamp::from_millisecond(at.get())
        .map(|t| t.to_zoned(zone.clone()).strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn numbers(members: impl IntoIterator<Item = MemberNo>) -> String {
    members
        .into_iter()
        .map(|n| format!("member:{}", n.get()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The group's names as prompts may show them.
///
/// A name others use for a member is shown when something ties it to them: a person set it,
/// the member gave it or answered to it, or it is part of a display name of theirs. Without
/// such an anchor it is one model's reading of a line, so it is kept but not shown until
/// [`CORROBORATED_USES`] conversations used it for that member and no other person has a claim to
/// it (an anchor, as many conversations, or it being their display name).
#[derive(Debug, Default)]
pub(crate) struct NameIndex {
    /// The names shown, by account and key.
    shown: BTreeSet<(AccountId, String)>,
    /// Which members each shown name (and each listed member's current display name) stands for.
    owners: BTreeMap<String, BTreeSet<MemberNo>>,
}

impl NameIndex {
    pub(crate) fn of(people: &GroupPeople) -> Self {
        // The person each account belongs to (linked accounts are one), and the display names
        // each account's lines were sent under.
        let mut person: BTreeMap<AccountId, usize> = BTreeMap::new();
        let mut displayed: BTreeMap<AccountId, Vec<String>> = BTreeMap::new();
        let mut owners: BTreeMap<String, BTreeSet<MemberNo>> = BTreeMap::new();
        // Display names that are someone's now, by key: a claim to the name like any other.
        let mut current: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for (index, p) in people.people.iter().enumerate() {
            let mut latest: BTreeSet<MemberNo> = BTreeSet::new();
            for (number, account) in &p.members {
                person.insert(*account, index);
                for shown in p.shown.iter().filter(|s| s.member == *number) {
                    let Ok(key) = name_key(&shown.text) else {
                        continue;
                    };
                    // Shown names are the most recent first: a member's first is the current one.
                    if latest.insert(shown.member) {
                        owners.entry(key.clone()).or_default().insert(*number);
                        current.entry(key.clone()).or_default().insert(index);
                    }
                    displayed.entry(*account).or_default().push(key);
                }
            }
        }
        // Accounts the roster does not list are each their own person.
        let unlisted = people.people.len();
        let person_of = |account: AccountId| {
            person.get(&account).copied().unwrap_or_else(|| {
                unlisted
                    + people
                        .names
                        .iter()
                        .position(|(_, n)| n.account == account)
                        .unwrap_or_default()
            })
        };
        let anchored = |name: &KnownName| {
            name.vouched()
                || (name.key.chars().count() >= 2
                    && displayed
                        .get(&name.account)
                        .is_some_and(|keys| keys.iter().any(|k| k.contains(&name.key))))
        };
        let strong = |name: &KnownName| anchored(name) || name.uses() >= CORROBORATED_USES;
        let mut shown = BTreeSet::new();
        for (number, name) in &people.names {
            let me = person_of(name.account);
            let contested = people.names.iter().any(|(_, other)| {
                other.key == name.key && person_of(other.account) != me && strong(other)
            }) || current
                .get(&name.key)
                .is_some_and(|theirs| theirs.iter().any(|p| *p != me));
            if anchored(name) || (name.uses() >= CORROBORATED_USES && !contested) {
                shown.insert((name.account, name.key.clone()));
                owners.entry(name.key.clone()).or_default().insert(*number);
            }
        }
        Self { shown, owners }
    }

    /// Whether prompts show this name for its member.
    pub(crate) fn shows(&self, name: &KnownName) -> bool {
        self.shown.contains(&(name.account, name.key.clone()))
    }
}

/// How a name is used for a member: set by a person, given by them, used by others, and the
/// other members it is also used for.
pub(crate) fn name_entry(
    zone: &TimeZone,
    name: &KnownName,
    index: &NameIndex,
    own_members: &[MemberNo],
) -> String {
    let mut how = Vec::new();
    if name.set {
        how.push(say(Text::PromptNameSet {}));
    }
    if name.own > 0 {
        how.push(say(Text::PromptNameOwn {
            count: name.own.to_string(),
        }));
    }
    let others = name.addressed + name.about;
    if others > 0 {
        how.push(say(Text::PromptNameOthers {
            count: others.to_string(),
            date: date(zone, name.last),
        }));
    }
    let shared: Vec<MemberNo> = index
        .owners
        .get(&name.key)
        .into_iter()
        .flatten()
        .copied()
        .filter(|n| !own_members.contains(n))
        .collect();
    if !shared.is_empty() {
        how.push(say(Text::PromptNameShared {
            members: numbers(shared),
        }));
    }
    say(Text::PromptNameEntry {
        name: line(&name.text),
        how: how.join("; "),
    })
}

/// The names of a person worth listing beside what they are shown as: every set name, and
/// learned ones the index shows that are not merely a display name of theirs.
pub(crate) fn names_beyond_shown<'a>(person: &'a Person, index: &NameIndex) -> Vec<&'a KnownName> {
    let shown: BTreeSet<String> = person
        .shown
        .iter()
        .filter_map(|s| name_key(&s.text).ok())
        .collect();
    person
        .names
        .iter()
        .filter(|n| index.shows(n) && (n.set || !shown.contains(&n.key)))
        .collect()
}

/// One roster entry; `None` when nothing is known beyond the member number.
pub(crate) fn roster_entry(zone: &TimeZone, person: &Person, index: &NameIndex) -> Option<String> {
    let members: Vec<MemberNo> = person.members.iter().map(|(n, _)| *n).collect();
    let mut out = vec![say(if members.len() > 1 {
        Text::PromptPersonLinked {
            members: numbers(members.iter().copied()),
        }
    } else {
        Text::PromptPerson {
            members: numbers(members.iter().copied()),
        }
    })];
    let mut known = members.len() > 1;
    if let Some(current) = person.shown.first() {
        out.push(say(Text::PromptPersonShown {
            name: line(&current.text),
        }));
        let earlier: Vec<String> = person.shown[1..]
            .iter()
            .filter(|s| s.text != current.text)
            .map(|s| format!("\"{}\"", line(&s.text)))
            .collect();
        if !earlier.is_empty() {
            out.push(say(Text::PromptPersonShownBefore {
                names: earlier.join(", "),
            }));
        }
        known = true;
    }
    let names: Vec<String> = names_beyond_shown(person, index)
        .into_iter()
        .map(|n| name_entry(zone, n, index, &members))
        .collect();
    if !names.is_empty() {
        out.push(say(Text::PromptPersonCalled {
            names: names.join(", "),
        }));
        known = true;
    }
    if !person.blocked.is_empty() {
        out.push(say(Text::PromptPersonBlocked {
            members: numbers(person.blocked.iter().copied()),
        }));
        known = true;
    }
    known.then(|| out.join("\n"))
}

/// Who said a memory: for a member, they themselves, others (to them), or both; for the group,
/// its members.
pub(crate) fn source(memory: &Memory) -> String {
    if memory.subject.is_none() {
        return say(Text::PromptMemoryMembers {});
    }
    let own = memory.own_word();
    say(if own == 0 {
        Text::PromptMemoryOthers {}
    } else if own == memory.episodes() {
        Text::PromptMemoryOwn {}
    } else {
        Text::PromptMemoryBoth {}
    })
}

/// A memory as a reply reads it, with its provenance.
pub(crate) fn memory_item(zone: &TimeZone, memory: &Memory) -> String {
    say(Text::PromptMemoryItem {
        text: line(&memory.display()),
        source: source(memory),
        count: memory.episodes().to_string(),
        date: date(zone, memory.last),
    })
}

/// A group memory as the learned-knowledge block reads it.
pub(crate) fn group_item(zone: &TimeZone, memory: &Memory) -> String {
    let (count, date) = (memory.episodes().to_string(), date(zone, memory.last));
    match &memory.term {
        Some(term) => say(Text::PromptGroupTerm {
            term: line(term),
            text: line(&memory.text),
            count,
            date,
        }),
        None => say(Text::PromptGroupMemory {
            text: line(&memory.text),
            count,
            date,
        }),
    }
}

/// A memory as the extractor reads it: with its id, so a statement can name it.
pub(crate) fn background_item(zone: &TimeZone, memory: &Memory) -> String {
    say(Text::ExtractBackgroundMemory {
        id: memory.id.label(),
        durability: memory.durability.as_str().to_owned(),
        text: line(&memory.display()),
        source: source(memory),
        count: memory.episodes().to_string(),
        date: date(zone, memory.last),
    })
}
