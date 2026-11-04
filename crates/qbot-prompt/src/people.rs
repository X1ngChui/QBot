//! How what is kept about people reads to a model: the roster (who is shown as what, the names
//! people use for whom), memories with their provenance, notes. Shared by the reply prompt, the
//! extractor's background and `about_member`, so all three say the same thing the same way.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::marker::escape_markers;
use qbot_core::{MemberNo, UnixMillis};
use qbot_memory::memories::Memory;
use qbot_memory::names::{KnownName, name_key};
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

/// Which members each name (by key) stands for in the group: the names people use and what each
/// listed member is shown as now.
pub(crate) fn name_owners(people: &GroupPeople) -> BTreeMap<String, BTreeSet<MemberNo>> {
    let mut owners: BTreeMap<String, BTreeSet<MemberNo>> = BTreeMap::new();
    for (number, name) in &people.names {
        owners.entry(name.key.clone()).or_default().insert(*number);
    }
    for person in &people.people {
        let mut latest: BTreeSet<MemberNo> = BTreeSet::new();
        for shown in &person.shown {
            // Shown names are the most recent first: each member's first is the current one.
            if latest.insert(shown.member)
                && let Ok(key) = name_key(&shown.text)
            {
                owners.entry(key).or_default().insert(shown.member);
            }
        }
    }
    owners
}

/// How a name is used for a member: set by a person, given by them, used by others, and the
/// other members it is also used for.
pub(crate) fn name_entry(
    zone: &TimeZone,
    name: &KnownName,
    owners: &BTreeMap<String, BTreeSet<MemberNo>>,
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
    let shared: Vec<MemberNo> = owners
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
/// learned ones that are not merely a display name of theirs.
pub(crate) fn names_beyond_shown(person: &Person) -> Vec<&KnownName> {
    let shown: BTreeSet<String> = person
        .shown
        .iter()
        .filter_map(|s| name_key(&s.text).ok())
        .collect();
    person
        .names
        .iter()
        .filter(|n| n.set || !shown.contains(&n.key))
        .collect()
}

/// One roster entry; `None` when nothing is known beyond the member number.
pub(crate) fn roster_entry(
    zone: &TimeZone,
    person: &Person,
    owners: &BTreeMap<String, BTreeSet<MemberNo>>,
) -> Option<String> {
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
    let names: Vec<String> = names_beyond_shown(person)
        .into_iter()
        .map(|n| name_entry(zone, n, owners, &members))
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
