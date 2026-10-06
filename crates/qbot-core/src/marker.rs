//! The bracketed markers archived chat text carries: `[at:3]`, `[image]`, `[image:a cat]`.
//!
//! The gateway writes them, the media service fills them in later, and the prompt layer explains
//! them. The one rule that makes them trustworthy: member-typed square brackets never reach the
//! archive as ASCII brackets ([`neutralize`]), so every ASCII bracket in archived text is a real
//! marker.

/// The kinds of media a line can carry that the system fetches and fills in later. Each is a
/// marker name in archived text (`[image]`, `[sticker]`, `[voice]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    /// A marketplace sticker: described like a picture, cached by its sticker id.
    Sticker,
    Voice,
}

impl MediaKind {
    /// The marker name, also the stored form.
    pub fn marker(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Sticker => "sticker",
            MediaKind::Voice => "voice",
        }
    }

    /// The kind a marker name stands for.
    pub fn from_marker(name: &str) -> Option<Self> {
        match name {
            "image" => Some(MediaKind::Image),
            "sticker" => Some(MediaKind::Sticker),
            "voice" => Some(MediaKind::Voice),
            _ => None,
        }
    }
}

/// A hand in the platform's rock-paper-scissors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpsHand {
    Rock,
    Paper,
    Scissors,
}

impl RpsHand {
    pub fn name(self) -> &'static str {
        match self {
            RpsHand::Rock => "rock",
            RpsHand::Paper => "paper",
            RpsHand::Scissors => "scissors",
        }
    }
}

/// What a platform game came up with: a dice roll or a rock-paper-scissors hand.
///
/// Only the incoming side carries one. The platform picks it when it performs the action, and
/// the bot learns it from the echo of its own message; a request (`[dice]`, `[rps]`) never has a
/// result. The archived form `[dice result:4]` / `[rps result:rock]` is deliberately different
/// from the request markers, so a result reads as something observed, not something to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameResult {
    /// The face rolled, 1 to 6.
    Dice(u8),
    Rps(RpsHand),
}

impl GameResult {
    /// A dice result as the platform reports it (`"1"` to `"6"`).
    pub fn dice(raw: &str) -> Option<Self> {
        raw.trim()
            .parse::<u8>()
            .ok()
            .filter(|face| (1..=6).contains(face))
            .map(GameResult::Dice)
    }

    /// A rock-paper-scissors result as QQ reports it: 1 paper, 2 scissors, 3 rock (the order
    /// of QQ's own label for the move, "cloth, shears, hammer").
    pub fn rps(raw: &str) -> Option<Self> {
        let hand = match raw.trim() {
            "1" => RpsHand::Paper,
            "2" => RpsHand::Scissors,
            "3" => RpsHand::Rock,
            _ => return None,
        };
        Some(GameResult::Rps(hand))
    }

    /// The archived marker: `[dice result:4]`, `[rps result:rock]`.
    pub fn marker(self) -> String {
        match self {
            GameResult::Dice(face) => format!("[dice result:{face}]"),
            GameResult::Rps(hand) => format!("[rps result:{}]", hand.name()),
        }
    }
}

const FULLWIDTH_OPEN: char = '\u{FF3B}';
const FULLWIDTH_CLOSE: char = '\u{FF3D}';

/// Text typed by a member (or produced from outside the system) made safe to archive: no NUL
/// (the database rejects it) and no ASCII brackets, so it cannot imitate a marker.
pub fn neutralize(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '[' => FULLWIDTH_OPEN,
            ']' => FULLWIDTH_CLOSE,
            other => other,
        })
        .collect()
}

/// Replace the `index`-th (0-based) marker named `name` in `text`, whether it is bare (`[image]`)
/// or already filled (`[image:...]`), with `replacement`. `None` when there is no such marker.
///
/// Counting includes filled markers, so a message's slots keep their numbers as they are filled
/// in any order.
pub fn fill(text: &str, name: &str, index: usize, replacement: &str) -> Option<String> {
    let bare = format!("[{name}]");
    let open = format!("[{name}:");
    let mut seen = 0;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let len = if rest.starts_with(&bare) {
            bare.len()
        } else if rest.starts_with(&open) {
            // Filled contents never contain ASCII brackets (see `neutralize`), so the first `]`
            // closes the marker.
            rest.find(']').map(|close| close + 1)?
        } else {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        if seen == index {
            return Some(format!(
                "{}{}{}",
                &text[..at],
                replacement,
                &text[at + len..]
            ));
        }
        seen += 1;
        at += len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_game_results_read_as_observed_results() {
        assert_eq!(
            GameResult::dice(" 4 ").map(GameResult::marker).as_deref(),
            Some("[dice result:4]")
        );
        for out_of_range in ["0", "7", "-1", "six", ""] {
            assert_eq!(GameResult::dice(out_of_range), None, "{out_of_range:?}");
        }
        // QQ's numbering: 1 paper, 2 scissors, 3 rock.
        let hands: Vec<_> = ["1", "2", "3"]
            .into_iter()
            .map(|raw| GameResult::rps(raw).map(GameResult::marker))
            .collect();
        assert_eq!(
            hands,
            [
                Some("[rps result:paper]".to_owned()),
                Some("[rps result:scissors]".to_owned()),
                Some("[rps result:rock]".to_owned())
            ]
        );
        assert_eq!(GameResult::rps("0"), None);
        assert_eq!(GameResult::rps("4"), None);
    }

    #[test]
    fn neutralize_removes_nul_and_ascii_brackets() {
        assert_eq!(neutralize("a[b]\0c"), "a\u{FF3B}b\u{FF3D}c");
    }

    #[test]
    fn fill_counts_bare_and_filled_markers_alike() {
        let text = "x [image] y [voice] z [image:a cat] w [image]";
        assert_eq!(
            fill(text, "image", 0, "[image:one]").as_deref(),
            Some("x [image:one] y [voice] z [image:a cat] w [image]")
        );
        assert_eq!(
            fill(text, "image", 1, "[image:two]").as_deref(),
            Some("x [image] y [voice] z [image:two] w [image]"),
            "a filled marker keeps its slot"
        );
        assert_eq!(
            fill(text, "image", 2, "[image:three]").as_deref(),
            Some("x [image] y [voice] z [image:a cat] w [image:three]")
        );
        assert_eq!(fill(text, "image", 3, "[image:none]"), None);
        assert_eq!(
            fill(text, "voice", 0, "[voice:hi]").as_deref(),
            Some("x [image] y [voice:hi] z [image:a cat] w [image]")
        );
        assert_eq!(fill("no markers", "image", 0, "[image:x]"), None);
        // Multi-byte text before a marker does not confuse the scan.
        assert_eq!(
            fill("\u{4F60}\u{597D}[image]", "image", 0, "[image:ok]").as_deref(),
            Some("\u{4F60}\u{597D}[image:ok]")
        );
    }
}
