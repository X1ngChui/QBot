//! What a run is shown of a group's history.
//!
//! A group's archived lines are numbered by a dense per-group *ordinal* starting at 1. The
//! group's batch (memory's nightly work) turns everything archived so far into episodes. Between
//! two batches a run is shown:
//!
//! - the summaries of the `summaries` newest episodes before the raw part;
//! - the raw part: the `carry_lines` lines before the batch's cut, from the start of the episode
//!   holding the first of them, then every line archived since the cut.
//!
//! Nothing in this view moves until the next batch: the raw part only grows, so every run shares
//! the prefix of the one before it. When a provider rejects a request as too long, the run moves
//! the start of the raw part forward (see `qbot-agent`), and that cut holds until the next batch.

/// How much history a run is shown around the group's last batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryWindow {
    /// Episode summaries shown before the raw part.
    pub summaries: u32,
    /// Lines before the batch's cut shown raw, so a run just after a batch still sees the
    /// conversation it continues.
    pub carry_lines: u32,
}

impl Default for HistoryWindow {
    fn default() -> Self {
        Self {
            summaries: 8,
            carry_lines: 90,
        }
    }
}

impl HistoryWindow {
    /// The first ordinal of the carried lines for a batch cut after `covered_through` (0: no
    /// cut yet), before it is moved back to the start of the episode holding it.
    pub fn carry_from(self, covered_through: u64) -> u64 {
        covered_through
            .saturating_sub(u64::from(self.carry_lines))
            .saturating_add(1)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_carried_lines_end_at_the_cut() {
        let w = HistoryWindow {
            summaries: 8,
            carry_lines: 90,
        };
        assert_eq!(w.carry_from(0), 1, "no batch yet: everything is raw");
        assert_eq!(w.carry_from(50), 1);
        assert_eq!(w.carry_from(300), 211);
        let none = HistoryWindow {
            carry_lines: 0,
            ..w
        };
        assert_eq!(
            none.carry_from(300),
            301,
            "nothing carried: the raw part starts after the cut"
        );
    }
}
