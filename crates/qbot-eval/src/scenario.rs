//! The scenario file format. A scenario is one group situation: who is in the chat, what was
//! said, what started the run, what the bot may look up, and what it should (and must not) do.
//! Descriptions and rubrics are English; chat lines may be in any language.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// What the situation is and why it matters, for the report and the judge.
    pub description: String,
    /// Local time of the first chat line (RFC 3339 with offset).
    #[serde(default = "default_start")]
    pub start: String,
    pub members: Vec<Member>,
    /// Sets of member numbers whose accounts are linked as one person.
    #[serde(default)]
    pub same_person: Vec<Vec<u32>>,
    pub chat: Vec<Line>,
    pub trigger: TriggerSpec,
    #[serde(default)]
    pub notes: Vec<NoteSpec>,
    #[serde(default)]
    pub facts: Vec<FactSpec>,
    #[serde(default)]
    pub knowledge: Vec<KnowledgeSpec>,
    /// What every `web_search` returns.
    #[serde(default)]
    pub web: Vec<WebHit>,
    /// What `read_url` returns, in order of calls.
    #[serde(default)]
    pub pages: Vec<String>,
    pub expect: Expect,
}

fn default_start() -> String {
    "2026-10-05T20:00:00+08:00".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// The member number the model sees (`member:N`).
    pub number: u32,
    pub account: i64,
    /// The member's current group display name.
    pub name: String,
    #[serde(default)]
    pub blocked: bool,
    /// The platform cannot say this member's name (the directory answers nothing).
    #[serde(default)]
    pub name_unavailable: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    /// The speaker's member number; absent for the bot's own earlier lines.
    #[serde(default)]
    pub member: Option<u32>,
    pub text: String,
    /// Minutes after the previous line (default 1).
    #[serde(default)]
    pub minutes: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    /// The chat line (1-based) whose sender addressed the bot.
    #[serde(default)]
    pub line: Option<usize>,
    /// A scheduled task coming due, with its stored intent.
    #[serde(default)]
    pub wake: Option<String>,
    /// The bot looks at the chat on its own: nobody addressed it.
    #[serde(default)]
    pub spontaneous: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteSpec {
    pub member: u32,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactSpec {
    pub member: u32,
    pub predicate: String,
    pub object: String,
    /// How many episodes support it (more is more confident).
    #[serde(default = "one")]
    pub episodes: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSpec {
    /// A term the group uses; absent for the group's topic.
    #[serde(default)]
    pub term: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebHit {
    pub title: String,
    pub url: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendRule {
    /// The bot must send at least one message.
    Required,
    /// The bot must stay silent.
    Forbidden,
    /// Either is acceptable; the rubric decides.
    Optional,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub send: SendRule,
    #[serde(default)]
    pub tools_required: Vec<String>,
    #[serde(default)]
    pub tools_forbidden: Vec<String>,
    /// Member numbers the bot must not @-mention or share a card of.
    #[serde(default)]
    pub mentions_forbidden: Vec<u32>,
    /// The bot may write `member:N` in its messages (someone asked about the numbers). Off by
    /// default: numbers are internal handles, not names.
    #[serde(default)]
    pub member_handles_allowed: bool,
    /// English criteria for the judge, each checkable from the transcript.
    #[serde(default)]
    pub rubric: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    #[error("{0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, ScenarioError> {
        let scenario: Scenario = toml::from_str(text)?;
        scenario.check()?;
        Ok(scenario)
    }

    fn check(&self) -> Result<(), ScenarioError> {
        let invalid = |m: String| Err(ScenarioError::Invalid(m));
        let known = |n: u32| self.members.iter().any(|m| m.number == n);
        for (i, line) in self.chat.iter().enumerate() {
            if let Some(n) = line.member
                && !known(n)
            {
                return invalid(format!("chat line {} names unknown member {n}", i + 1));
            }
        }
        match (
            &self.trigger.line,
            &self.trigger.wake,
            self.trigger.spontaneous,
        ) {
            (Some(n), None, false) => {
                match self.chat.get(n.wrapping_sub(1)).and_then(|l| l.member) {
                    Some(_) => {}
                    None => return invalid(format!("trigger line {n} is not a member's line")),
                }
            }
            (None, Some(_), false) | (None, None, true) => {}
            _ => {
                return invalid(
                    "trigger needs exactly one of `line`, `wake` or `spontaneous = true`".into(),
                );
            }
        }
        for n in self
            .notes
            .iter()
            .map(|n| n.member)
            .chain(self.facts.iter().map(|f| f.member))
            .chain(self.expect.mentions_forbidden.iter().copied())
        {
            if !known(n) {
                return invalid(format!("unknown member {n}"));
            }
        }
        if jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%:z", &self.start).is_err() {
            return invalid(format!(
                "start {:?} is not RFC 3339 with an offset",
                self.start
            ));
        }
        Ok(())
    }

    pub fn member(&self, number: u32) -> Option<&Member> {
        self.members.iter().find(|m| m.number == number)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
description = "A member asks something."
members = [{ number = 1, account = 1001, name = "Ann" }]
chat = [{ member = 1, text = "hi bot" }]
trigger = { line = 1 }
expect = { send = "required", rubric = ["Greets back."] }
"#;

    #[test]
    fn a_minimal_scenario_parses_and_bad_references_are_refused() {
        let s = Scenario::parse(MINIMAL).unwrap();
        assert_eq!(s.expect.send, SendRule::Required);
        assert_eq!(s.member(1).unwrap().name, "Ann");
        for bad in [
            MINIMAL.replace("line = 1", "line = 2"),
            MINIMAL.replace("trigger = { line = 1 }", "trigger = {}"),
            MINIMAL.replace("chat = [{ member = 1", "chat = [{ member = 9"),
            MINIMAL.replace("rubric", "rubrics"),
        ] {
            assert!(Scenario::parse(&bad).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod shipped {
    use super::*;

    #[test]
    fn every_shipped_scenario_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/scenarios");
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|x| x == "toml") {
                let text = std::fs::read_to_string(&path).unwrap();
                Scenario::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                count += 1;
            }
        }
        assert!(count > 0, "no scenarios in {}", dir.display());
    }
}
