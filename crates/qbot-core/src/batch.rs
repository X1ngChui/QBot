//! The chat-batch grid.
//!
//! A group's archived lines are numbered by a dense per-group *ordinal* starting at 1. History is
//! organized in fixed-size *batches* of consecutive ordinals, and old history is evicted a whole
//! batch at a time so the prompt prefix changes rarely. Memory slices follow the same grid: a
//! slice is a fixed number of whole batches, so episode boundaries are exactly the boundaries the
//! history window already has.

use std::ops::RangeInclusive;

/// What a run is shown of the group's history, in whole batches, counted back from the batch
/// being filled:
///
/// - the newest `raw_batches` (including the one being filled) verbatim;
/// - the `summary_batches` before them as the summaries of the episodes that end there (lines
///   no episode covers yet stay verbatim);
/// - nothing older: that history is reached only through the memory and history tools.
///
/// The boundaries move a batch at a time, so the prompt changes only when a batch fills. These are
/// line counts, not a token budget; a provider's context limit is not the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryWindow {
    pub batch_lines: u32,
    pub raw_batches: u32,
    pub summary_batches: u32,
}

impl Default for HistoryWindow {
    fn default() -> Self {
        Self {
            batch_lines: 30,
            raw_batches: 4,
            summary_batches: 9,
        }
    }
}

/// Where a window's tiers begin, as batch indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowTiers {
    /// The batch being filled.
    pub current: u64,
    /// The oldest batch shown verbatim.
    pub raw_from: u64,
    /// The oldest batch shown through summaries; equal to `raw_from` when there is no summary
    /// tier.
    pub summary_from: u64,
}

impl HistoryWindow {
    pub fn grid(self) -> BatchGrid {
        BatchGrid {
            lines_per_batch: self.batch_lines,
        }
    }

    /// The tiers for an archive whose newest line has `last_ordinal` (at least 1).
    pub fn tiers(self, last_ordinal: u64) -> WindowTiers {
        let current = self.grid().batch_of(last_ordinal.max(1));
        let raw_from = current.saturating_sub(u64::from(self.raw_batches.max(1)) - 1);
        let summary_from = raw_from.saturating_sub(u64::from(self.summary_batches));
        WindowTiers {
            current,
            raw_from,
            summary_from,
        }
    }
}

/// Ordinals to batches. Batch indexes start at 0; batch `b` holds ordinals
/// `b * n + 1 ..= (b + 1) * n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchGrid {
    pub lines_per_batch: u32,
}

impl BatchGrid {
    fn n(self) -> u64 {
        u64::from(self.lines_per_batch.max(1))
    }

    /// The batch an ordinal (1-based) belongs to.
    pub fn batch_of(self, ordinal: u64) -> u64 {
        ordinal.saturating_sub(1) / self.n()
    }

    pub fn batch_range(self, batch: u64) -> RangeInclusive<u64> {
        batch * self.n() + 1..=(batch + 1) * self.n()
    }

    /// Whole batches covered by `ordinals`, counting only batches that are complete.
    pub fn complete_batches(self, last_ordinal: u64) -> u64 {
        last_ordinal / self.n()
    }
}

/// How the archive is cut into episodes, and how much surrounding chat each extraction may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceGrid {
    pub grid: BatchGrid,
    /// Batches per slice: the unit of one episode.
    pub slice_batches: u32,
    /// Batches shown before the slice as context only (fewer near the start of the archive).
    pub previous_context_batches: u32,
    /// Batches shown after the slice as context only. Bounded by the retained raw window and by
    /// what has been produced so far; see [`SliceGrid::effective_next_batches`].
    pub next_context_batches: u32,
    /// Batches shown verbatim ([`HistoryWindow::raw_batches`]): a slice's episode should exist
    /// by the time its first batch leaves that tier.
    pub retained_raw_batches: u32,
}

/// The range an episode will own, and the context around it. Context is never part of the range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicePlan {
    pub first_batch: u64,
    pub last_batch: u64,
    /// Ordinals the episode owns.
    pub target: RangeInclusive<u64>,
    /// The nearest earlier batches, as context only; absent for the first slice or when none is
    /// configured.
    pub previous: Option<RangeInclusive<u64>>,
    /// The nearest following batches that exist so far, as context only; absent when none is
    /// configured or none has been produced. Possibly fewer than configured.
    pub next: Option<RangeInclusive<u64>>,
}

impl SliceGrid {
    /// Slices of `slice_batches` over `window`, each read with one batch of context on either
    /// side: enough to resolve what crosses a slice's edge, without paying for a second slice.
    pub fn for_window(window: HistoryWindow, slice_batches: u32) -> Self {
        Self::from_window(window, slice_batches, 1, 1)
    }

    pub fn from_window(
        window: HistoryWindow,
        slice_batches: u32,
        previous_context_batches: u32,
        next_context_batches: u32,
    ) -> Self {
        Self {
            grid: window.grid(),
            slice_batches,
            previous_context_batches,
            next_context_batches,
            retained_raw_batches: window.raw_batches,
        }
    }

    /// How many following batches an extraction can count on. To read `n` batches after a slice,
    /// the slice and those `n` batches must all still be raw at the same moment, or the slice
    /// would be evicted before its episode exists. So the request is capped by what the
    /// retained window leaves after the slice.
    pub fn effective_next_batches(self) -> u32 {
        self.next_context_batches
            .min(self.retained_raw_batches.saturating_sub(self.slice_batches))
    }

    /// The next slice to extract, given the last ordinal already covered by an episode (0 for
    /// none) and the last archived ordinal, or `None` until all of its batches are complete.
    ///
    /// A slice starts at the first batch boundary at or after the covered point. Extraction does
    /// not wait for following context: it uses as many complete following batches as exist, up to
    /// [`SliceGrid::effective_next_batches`], and none if none has been produced yet.
    pub fn next(self, covered_through: u64, archive_last: u64) -> Option<SlicePlan> {
        let n = self.grid.n();
        let slice = u64::from(self.slice_batches.max(1));
        let first_batch = covered_through.div_ceil(n);
        let last_batch = first_batch + slice - 1;

        let complete = self.grid.complete_batches(archive_last);
        if complete <= last_batch {
            return None;
        }

        let previous_batches = u64::from(self.previous_context_batches).min(first_batch);
        let previous = (previous_batches > 0).then(|| {
            *self
                .grid
                .batch_range(first_batch - previous_batches)
                .start()..=*self.grid.batch_range(first_batch - 1).end()
        });
        let following = (complete - (last_batch + 1)).min(u64::from(self.effective_next_batches()));
        let next = (following > 0).then(|| {
            *self.grid.batch_range(last_batch + 1).start()
                ..=*self.grid.batch_range(last_batch + following).end()
        });
        Some(SlicePlan {
            first_batch,
            last_batch,
            target: *self.grid.batch_range(first_batch).start()
                ..=*self.grid.batch_range(last_batch).end(),
            previous,
            next,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_tiers_count_whole_batches_back_from_the_one_being_filled() {
        let w = HistoryWindow {
            batch_lines: 10,
            raw_batches: 2,
            summary_batches: 3,
        };
        // Line 95 is in batch 9: batches 8-9 verbatim, 5-7 summarized, 0-4 left out.
        assert_eq!(
            w.tiers(95),
            WindowTiers {
                current: 9,
                raw_from: 8,
                summary_from: 5
            }
        );
        // Near the start nothing underflows and every tier may be short or empty.
        assert_eq!(
            w.tiers(12),
            WindowTiers {
                current: 1,
                raw_from: 0,
                summary_from: 0
            }
        );
        let none = HistoryWindow {
            summary_batches: 0,
            ..w
        };
        assert_eq!(none.tiers(95).summary_from, 8);
        // A batch boundary moves the tiers by exactly one batch.
        assert_eq!(w.tiers(100), w.tiers(91));
        assert_eq!(w.tiers(101).raw_from, 9);
    }

    #[test]
    fn ordinals_map_to_batches_on_a_one_based_grid() {
        let g = BatchGrid {
            lines_per_batch: 30,
        };
        assert_eq!(
            (
                g.batch_of(1),
                g.batch_of(30),
                g.batch_of(31),
                g.batch_of(90)
            ),
            (0, 0, 1, 2)
        );
        assert_eq!(g.batch_range(0), 1..=30);
        assert_eq!(g.batch_range(2), 61..=90);
        assert_eq!(
            (
                g.complete_batches(29),
                g.complete_batches(30),
                g.complete_batches(95)
            ),
            (0, 1, 3)
        );
    }

    fn slices(batch: u32, slice: u32, previous: u32, next: u32, retained: u32) -> SliceGrid {
        SliceGrid {
            grid: BatchGrid {
                lines_per_batch: batch,
            },
            slice_batches: slice,
            previous_context_batches: previous,
            next_context_batches: next,
            retained_raw_batches: retained,
        }
    }

    #[test]
    fn slices_know_how_many_batches_stay_verbatim() {
        let s = SliceGrid::from_window(HistoryWindow::default(), 2, 1, 1);
        assert_eq!((s.grid.lines_per_batch, s.retained_raw_batches), (30, 4));
    }

    #[test]
    fn a_slice_is_ready_once_its_own_batches_are_complete() {
        let s = slices(10, 3, 1, 1, 100);
        assert_eq!(s.next(0, 29), None, "the third batch is not complete yet");
        let plan = s.next(0, 30).unwrap();
        assert_eq!((plan.first_batch, plan.last_batch), (0, 2));
        assert_eq!(plan.target, 1..=30);
        assert_eq!(plan.previous, None, "nothing precedes the first slice");
        assert_eq!(
            plan.next, None,
            "no following batch has been produced yet: extraction does not wait"
        );

        let partial = s.next(0, 35).unwrap();
        assert_eq!(
            partial.next, None,
            "a partly filled batch is not used as context"
        );
        let full = s.next(0, 40).unwrap();
        assert_eq!(full.next, Some(31..=40));

        let second = s
            .next(30, 75)
            .unwrap_or_else(|| panic!("slice 2 needs 60 lines"));
        assert_eq!(second.target, 31..=60);
        assert_eq!(second.previous, Some(21..=30));
        assert_eq!(second.next, Some(61..=70));
    }

    #[test]
    fn the_amount_of_context_on_each_side_is_a_setting() {
        let s = slices(10, 2, 3, 2, 100);
        let plan = s.next(60, 200).unwrap(); // batches 6..=7
        assert_eq!(plan.target, 61..=80);
        assert_eq!(plan.previous, Some(31..=60), "three batches before");
        assert_eq!(plan.next, Some(81..=100), "two batches after");

        let early = slices(10, 2, 3, 2, 100).next(20, 200).unwrap(); // batches 2..=3
        assert_eq!(
            early.previous,
            Some(1..=20),
            "fewer when the archive does not reach back that far"
        );

        let none = slices(10, 2, 0, 0, 100).next(0, 200).unwrap();
        assert_eq!(
            (none.previous, none.next),
            (None, None),
            "zero means no context"
        );
    }

    #[test]
    fn following_context_uses_fewer_batches_when_fewer_have_been_produced() {
        let s = slices(10, 2, 0, 3, 100);
        let produced = |last: u64| s.next(0, last).unwrap().next;
        assert_eq!(produced(20), None);
        assert_eq!(produced(30), Some(21..=30), "one of three wanted");
        assert_eq!(
            produced(45),
            Some(21..=40),
            "two complete; the partly filled fifth batch does not count"
        );
        assert_eq!(produced(50), Some(21..=50), "all three");
        assert_eq!(produced(500), Some(21..=50), "never more than asked for");
    }

    #[test]
    fn following_context_is_capped_by_what_the_retained_window_leaves() {
        // A slice of 3 batches in a window that keeps 5 raw: at most 2 following batches can ever
        // coexist with the slice, however many are requested.
        let s = slices(10, 3, 1, 9, 5);
        assert_eq!(s.effective_next_batches(), 2);
        assert_eq!(s.next(0, 500).unwrap().next, Some(31..=50));
        // A slice as large as the window leaves no room for following context at all.
        let full = slices(10, 5, 1, 3, 5);
        assert_eq!(full.effective_next_batches(), 0);
        assert_eq!(full.next(0, 500).unwrap().next, None);
        // A slice larger than the window must not underflow.
        assert_eq!(slices(10, 8, 1, 3, 5).effective_next_batches(), 0);
    }

    #[test]
    fn slices_tile_the_archive_with_no_gap_and_no_overlap() {
        let s = slices(7, 2, 1, 1, 100);
        let (mut covered, mut ranges) = (0u64, Vec::new());
        while let Some(plan) = s.next(covered, 200) {
            covered = *plan.target.end();
            ranges.push(plan.target);
        }
        assert_eq!(*ranges[0].start(), 1);
        for pair in ranges.windows(2) {
            assert_eq!(*pair[0].end() + 1, *pair[1].start());
        }
        assert!(
            covered <= 200 && 200 - covered < 14 + 14,
            "only the unfinished tail is left"
        );
    }

    #[test]
    fn a_misaligned_covered_point_resumes_at_the_next_batch_boundary() {
        // For example after the batch size changed: whole batches only, never a split batch.
        let plan = slices(10, 1, 0, 0, 100).next(25, 100).unwrap();
        assert_eq!(plan.target, 31..=40);
    }
}
