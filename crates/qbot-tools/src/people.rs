//! What is known about a member, on demand. Facts and notes never sit in the prompt: they change
//! and would break the cached prefix, and most replies do not need them. The model asks when a
//! reply depends on who someone is.
//!
//! Notes and learned facts are listed apart and described differently, because they are
//! different things: a note is what a person wrote by hand (exact, but someone's statement); a
//! fact is what extraction inferred from chat (scored, decaying).

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{Directory, Effect, Tool, ToolCx, ToolError, ToolOutput};
use qbot_core::{MemberNo, UnixMillis};
use qbot_memory::facts::{DecayPolicy, Fact, FactStore};
use qbot_memory::identity::{Alias, AliasStatus, AliasTarget};
use qbot_memory::{IdentityStore, NoteStore};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupArgs {
    pub member: u32,
}

#[derive(Clone)]
pub struct LookupMember {
    /// The platform's current name for a member, read live.
    pub directory: Arc<dyn Directory>,
    pub identity: Arc<dyn IdentityStore>,
    pub facts: Arc<dyn FactStore>,
    pub notes: Arc<dyn NoteStore>,
    pub decay: DecayPolicy,
}

impl std::fmt::Debug for LookupMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupMember").finish_non_exhaustive()
    }
}

fn unavailable(error: impl std::fmt::Display) -> ToolError {
    ToolError::Unavailable(error.to_string())
}

fn age(now: UnixMillis, then: UnixMillis) -> String {
    match now.since(then).as_secs() / 86_400 {
        0 => say(Text::LookupMemberToday {}),
        1 => say(Text::LookupMemberOneDay {}),
        days => say(Text::LookupMemberDays {
            days: days.to_string(),
        }),
    }
}

fn names(aliases: &[Alias]) -> Vec<String> {
    aliases
        .iter()
        .map(|a| match a.status {
            AliasStatus::Confirmed => say(Text::LookupMemberNameConfirmed {
                name: a.text.clone(),
            }),
            _ => say(Text::LookupMemberNameLead {
                name: a.text.clone(),
                confidence: format!("{:.2}", a.confidence),
            }),
        })
        .collect()
}

impl LookupMember {
    fn fact_line(&self, fact: &Fact, now: UnixMillis) -> String {
        say(Text::LookupMemberFact {
            predicate: fact.predicate.clone(),
            object: fact.object.clone(),
            confidence: format!("{:.2}", self.decay.confidence(fact, now)),
            age: age(now, fact.last_confirmed),
        })
    }
}

#[async_trait]
impl Tool for LookupMember {
    type Args = LookupArgs;
    const NAME: &'static str = "lookup_member";

    fn description(&self) -> String {
        say(Text::LookupMemberDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("member", say(Text::LookupMemberParamMember {}))]
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, cx: &ToolCx<'_>, args: LookupArgs) -> Result<ToolOutput, ToolError> {
        let account = cx
            .view
            .account_of(MemberNo::new(args.member))
            .ok_or_else(|| {
                ToolError::InvalidArguments(say(Text::LookupMemberNoMember {
                    member: args.member.to_string(),
                }))
            })?;
        let now = cx.clock.now();
        let holder = self
            .identity
            .holder_of(account)
            .await
            .map_err(unavailable)?;
        let mut aliases = self
            .identity
            .names_of(cx.group, AliasTarget::Account(account))
            .await
            .map_err(unavailable)?;
        if let Some(holder) = &holder {
            aliases.extend(
                self.identity
                    .names_of(cx.group, AliasTarget::Holder(holder.id))
                    .await
                    .map_err(unavailable)?,
            );
        }
        // The person's notes and facts: every account linked to them.
        let accounts = holder.map_or_else(|| vec![account], |h| h.accounts);
        let notes = self
            .notes
            .notes(cx.group, &accounts)
            .await
            .map_err(unavailable)?;
        let mut facts = Vec::new();
        for linked in accounts {
            facts.extend(
                self.facts
                    .current(cx.group, Some(linked))
                    .await
                    .map_err(unavailable)?,
            );
        }
        // By predicate; within one, the most recently confirmed first.
        facts.sort_by(|a, b| {
            a.predicate
                .cmp(&b.predicate)
                .then(b.last_confirmed.cmp(&a.last_confirmed))
                .then(b.id.cmp(&a.id))
        });

        let mut out = vec![format!("member:{}", args.member)];
        // The platform's current answer comes first and outranks every stored name.
        out.push(match self.directory.display_name(cx.group, account).await {
            Some(name) => say(Text::LookupMemberShownAs { name }),
            None => say(Text::LookupMemberShownUnavailable {}),
        });
        let names = names(&aliases);
        out.push(if names.is_empty() {
            say(Text::LookupMemberNoOtherNames {})
        } else {
            say(Text::LookupMemberOtherNames {
                names: names.join(", "),
            })
        });
        if notes.is_empty() {
            out.push(say(Text::LookupMemberNoNotes {}));
        } else {
            out.push(say(Text::LookupMemberNotes {}));
            out.extend(notes.iter().map(|n| {
                say(Text::LookupMemberNote {
                    text: n.text.clone(),
                    age: age(now, n.updated),
                })
            }));
        }
        if facts.is_empty() {
            out.push(say(Text::LookupMemberNoLearned {}));
        } else {
            out.push(say(Text::LookupMemberLearned {}));
            out.extend(facts.iter().map(|f| self.fact_line(f, now)));
        }
        Ok(ToolOutput::text(out.join("\n")))
    }
}
