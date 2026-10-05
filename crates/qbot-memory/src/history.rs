//! Composing a model's view of older chat from episodes.
//!
//! This is where memory and context compaction meet. An episode owns exact whole batches and its
//! summary never changes, so older chat lying wholly inside episodes can be shown as those
//! summaries instead of raw lines while the recent tail stays raw. The composed view is stable
//! between requests, so the provider's prefix cache stays valid, and the archive remains the
//! authority: nothing is rewritten, only what the reader is shown.

use crate::episode::Episode;

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryPart<T> {
    /// Raw lines, in order.
    Lines(Vec<T>),
    /// An episode standing in for every line of its range that was in the window.
    Recap(Box<Episode>),
}

/// Replace lines covered by episodes with the episode, except the last `keep_tail` lines, which
/// always stay raw, and except episodes that only partly lie inside the window (their summary
/// would describe lines the reader cannot see).
///
/// `lines` are `(ordinal, line)` in ascending order; `episodes` are any episodes of the same
/// group. Every input line appears exactly once in the result, raw or through one recap.
pub fn compose<T: Clone>(
    lines: &[(u64, T)],
    episodes: &[Episode],
    keep_tail: usize,
) -> Vec<HistoryPart<T>> {
    let (Some(first), Some(last)) = (lines.first().map(|l| l.0), lines.last().map(|l| l.0)) else {
        return Vec::new();
    };
    let tail_start = lines.len().saturating_sub(keep_tail);
    let tail_ordinal = lines.get(tail_start).map_or(u64::MAX, |l| l.0);

    let mut usable: Vec<&Episode> = episodes
        .iter()
        .filter(|e| {
            e.episode.first_ordinal >= first
                && e.episode.last_ordinal <= last
                && e.episode.last_ordinal < tail_ordinal
        })
        .collect();
    usable.sort_by_key(|e| e.episode.first_ordinal);

    let mut parts: Vec<HistoryPart<T>> = Vec::new();
    let mut raw: Vec<T> = Vec::new();
    let mut next = 0;
    for (ordinal, line) in lines {
        while next < usable.len() && usable[next].episode.last_ordinal < *ordinal {
            next += 1;
        }
        match usable.get(next) {
            Some(e) if e.episode.first_ordinal <= *ordinal => {
                if !raw.is_empty() {
                    parts.push(HistoryPart::Lines(std::mem::take(&mut raw)));
                }
                if !matches!(parts.last(), Some(HistoryPart::Recap(prev)) if prev.id == e.id) {
                    parts.push(HistoryPart::Recap(Box::new((*e).clone())));
                }
            }
            _ => raw.push(line.clone()),
        }
    }
    if !raw.is_empty() {
        parts.push(HistoryPart::Lines(raw));
    }
    parts
}
