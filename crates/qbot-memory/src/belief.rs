//! Confidence, with one meaning for every belief (a fact, a group term or topic, a name): the
//! estimated probability that the claim holds now.
//!
//! A belief can be wrong in three independent ways, and its confidence is the product of the
//! chances that it is not: the statements behind it were wrong ([`credibility`]), it stopped
//! holding since it was last said ([`persistence`]), or something said later contradicts it
//! ([`contradicted`]). Confidence qualifies what is shown; nothing is hidden or deleted for having
//! little of it.
//!
//! Separately, [`salience`] says how current a claim still is in its group's conversation, on the
//! group's own clock ([`Activity`]); confidence times salience is what a claim is worth where room
//! is limited (what replies are shown, what a full scope keeps).

use std::time::Duration;

use qbot_core::{AccountId, UnixMillis};

/// How likely `statements` independent agreeing statements are to be right: the posterior mean
/// of a uniform Beta prior after that many agreeing observations (Laplace's rule of succession).
/// One statement gives 2/3, eight give 0.9; repeating a claim never makes it certain.
pub fn credibility(statements: u32) -> f64 {
    let n = f64::from(statements);
    (n + 1.0) / (n + 2.0)
}

/// The chance that a claim still holds `since` its last support, for a claim of a kind that
/// typically keeps holding unsaid for `half_life`.
pub fn persistence(half_life: Duration, since: UnixMillis, now: UnixMillis) -> f64 {
    let age = now.since(since).as_secs_f64();
    0.5_f64.powf(age / half_life.as_secs_f64().max(1.0))
}

/// What is left of a belief after a later, incompatible statement with credibility
/// `credibility`: it holds only if that statement was wrong.
pub fn contradicted(confidence: f64, credibility: f64) -> f64 {
    confidence * (1.0 - credibility)
}

/// The moment confidence is computed for in what every reply is shown: the start of `now`'s UTC
/// day, so ageing changes the cached prompt prefix at most once a day.
pub fn as_of_day(now: UnixMillis) -> UnixMillis {
    const DAY_MS: i64 = 86_400_000;
    UnixMillis::new(now.get() - now.get().rem_euclid(DAY_MS))
}

/// A confidence as whole tens of percent, the precision it is shown with.
pub fn percent(confidence: f64) -> u32 {
    // In 0..=100 by construction, so the cast is exact.
    ((confidence.clamp(0.0, 1.0) * 10.0).round() * 10.0) as u32
}

/// How current a claim still is in its group's conversation: halved for every `half_life`
/// episodes that went by without supporting it. It is not part of confidence (whether the claim
/// holds) but of its value for limited room ([`Activity`]).
pub fn salience(half_life_episodes: f64, episodes_since: u32) -> f64 {
    0.5_f64.powf(f64::from(episodes_since) / half_life_episodes.max(f64::MIN_POSITIVE))
}

/// A group's conversation as its clock for salience: when each of its episodes ended and who
/// took part, in chat order. A quiet group's clock barely moves, so what it knows stays current;
/// a busy group's moves with every episode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    episodes: Vec<(UnixMillis, Vec<AccountId>)>,
}

impl Activity {
    pub fn new(mut episodes: Vec<(UnixMillis, Vec<AccountId>)>) -> Self {
        episodes.sort_by_key(|(ended, _)| *ended);
        Self { episodes }
    }

    /// Episodes ended after `after` and no later than `until` that bear on a claim about
    /// `subject`: those the person took part in (a claim about someone who said nothing since
    /// has not been passed over), or for a claim about the group, every one.
    pub fn since(&self, subject: Option<AccountId>, after: UnixMillis, until: UnixMillis) -> u32 {
        let count = self
            .episodes
            .iter()
            .filter(|(ended, _)| *ended > after && *ended <= until)
            .filter(|(_, people)| subject.is_none_or(|s| people.contains(&s)))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}
