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
