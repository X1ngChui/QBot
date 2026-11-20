//! How what is kept about people reads to a model: the roster (who is shown as what, the names
//! people use for whom) and memories with their provenance (from chat, or entered by hand).
//! Shared by the reply prompt, the batch's background and `about_member`, so all three say the
//! same thing the same way.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::marker::escape_markers;
use qbot_core::{AccountId, MemberNo, UnixMillis};
use qbot_memory::memories::{Memory, Origin};
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

/// The group's names as prompts may show them.
///
/// A name others use for a member is shown when something ties it to them: a person entered
/// it, a ruling over its uses placed it, the member used it for themselves, the chat's structure
/// tied it to them, or it is part of a display name of theirs. Without such an anchor it is one
/// reading of a conversation, so it is kept but not shown until separate conversations agree on
/// it ([`KnownName::corroborated`]). Two members may go by the same name; which one a line means
/// is the conversation's to show.
#[derive(Debug, Default)]
pub(crate) struct NameIndex {
    /// The names shown, by account and key.
    shown: BTreeSet<(AccountId, String)>,
}

impl NameIndex {
    pub(crate) fn of(people: &GroupPeople) -> Self {
        // The display names each account's lines were sent under.
        let mut displayed: BTreeMap<AccountId, Vec<String>> = BTreeMap::new();
        for p in &people.people {
            for (number, account) in &p.members {
                for shown in p.shown.iter().filter(|s| s.member == *number) {
                    if let Ok(key) = name_key(&shown.text) {
                        displayed.entry(*account).or_default().push(key);
                    }
                }
            }
        }
        let anchored = |name: &KnownName| {
            name.vouched()
                || (name.key.chars().count() >= 2
                    && displayed
                        .get(&name.account)
                        .is_some_and(|keys| keys.iter().any(|k| k.contains(&name.key))))
        };
        let shown = people
            .names
            .iter()
            .map(|(_, name)| name)
            .filter(|name| anchored(name) || name.corroborated())
            .map(|name| (name.account, name.key.clone()))
            .collect();
        Self { shown }
    }

    /// Whether prompts show this name for its member.
    pub(crate) fn shows(&self, name: &KnownName) -> bool {
        self.shown.contains(&(name.account, name.key.clone()))
    }
}

/// How a name is used for a member: entered by a person, placed by a ruling, given by them,
/// used by others.
pub(crate) fn name_entry(zone: &TimeZone, name: &KnownName) -> String {
    let mut how = Vec::new();
    if name.set {
        how.push(say(Text::PromptNameSet {}));
    }
    if name.ruled {
        how.push(say(Text::PromptNameRuled {}));
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
        .map(|n| name_entry(zone, n))
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

/// Where a memory comes from: entered by hand (by its member or by someone else), or said in
/// chat: for a member, by they themselves, others (to them), or both; for the group, its
/// members.
pub(crate) fn source(memory: &Memory) -> String {
    if let Origin::Hand { by } = memory.origin {
        return say(if memory.subject == Some(by) {
            Text::PromptMemoryHandOwn {}
        } else {
            Text::PromptMemoryHandOthers {}
        });
    }
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
    if memory.origin.is_hand() {
        return say(Text::PromptMemoryHand {
            text: line(&memory.display()),
            source: source(memory),
            date: date(zone, memory.first),
        });
    }
    say(Text::PromptMemoryItem {
        text: line(&memory.display()),
        source: source(memory),
        count: memory.episodes().max(1).to_string(),
        date: date(zone, memory.last),
    })
}

/// A group memory as the learned-knowledge block reads it.
pub(crate) fn group_item(zone: &TimeZone, memory: &Memory) -> String {
    if memory.origin.is_hand() {
        let date = date(zone, memory.first);
        return match &memory.term {
            Some(term) => say(Text::PromptGroupHandTerm {
                term: line(term),
                text: line(&memory.text),
                date,
            }),
            None => say(Text::PromptGroupHand {
                text: line(&memory.text),
                date,
            }),
        };
    }
    let (count, date) = (
        memory.episodes().max(1).to_string(),
        date(zone, memory.last),
    );
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

/// A memory as the batch reads it: with its id.
pub(crate) fn background_item(zone: &TimeZone, memory: &Memory) -> String {
    if memory.origin.is_hand() {
        return say(Text::ExtractBackgroundHand {
            id: memory.id.label(),
            text: line(&memory.display()),
            source: source(memory),
        });
    }
    say(Text::ExtractBackgroundMemory {
        id: memory.id.label(),
        durability: memory.durability.as_str().to_owned(),
        text: line(&memory.display()),
        source: source(memory),
        count: memory.episodes().max(1).to_string(),
        date: date(zone, memory.last),
    })
}
