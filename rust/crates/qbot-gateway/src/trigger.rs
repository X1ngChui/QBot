//! Who gets an answer. The bot speaks when spoken to: an @, one of its nicknames as a whole
//! word, or a quote of one of its own lines. There is nothing to tune and no model involved.

use std::collections::BTreeMap;

use jieba_rs::Jieba;

/// Whole-word nickname matching on segmented text.
///
/// A nickname that merely appears inside a longer word is not being addressed, and in Chinese
/// there is no space to mark a word, so the text is segmented. Nicknames are usually coined
/// words outside the dictionary, so each is added to the dictionary first or the segmenter
/// would split it apart.
pub struct Nicknames {
    jieba: Jieba,
    /// Lower-cased nickname to the form the operator wrote.
    wanted: BTreeMap<String, String>,
}

impl std::fmt::Debug for Nicknames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nicknames")
            .field("wanted", &self.wanted.values().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Nicknames {
    pub fn new<S: AsRef<str>>(nicknames: &[S]) -> Self {
        let mut jieba = Jieba::new();
        let mut wanted = BTreeMap::new();
        for nick in nicknames {
            let nick = nick.as_ref().trim();
            if nick.is_empty() {
                continue;
            }
            let lower = nick.to_lowercase();
            jieba.add_word(&lower, Some(100_000), None);
            wanted.insert(lower, nick.to_owned());
        }
        Self { jieba, wanted }
    }

    /// The nickname that appears as a whole token, if any.
    pub fn hit(&self, text: &str) -> Option<&str> {
        if self.wanted.is_empty() {
            return None;
        }
        let lower = text.trim().to_lowercase();
        if !self.wanted.keys().any(|k| lower.contains(k.as_str())) {
            return None;
        }
        let cut = self.jieba.cut(&lower, true);
        let tokens: Vec<&str> = cut.iter().map(|t| t.word).collect();
        self.wanted
            .iter()
            .find(|(key, _)| tokens.contains(&key.as_str()) && clean_boundary(&lower, key))
            .map(|(_, original)| original.as_str())
    }
}

fn ascii_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// A nickname whose edge is an ASCII letter must not be carved out of a longer Latin word: a
/// nickname ending in "x" must not fire inside "xbox".
fn clean_boundary(text: &str, key: &str) -> bool {
    let (Some(first), Some(last)) = (key.chars().next(), key.chars().next_back()) else {
        return false;
    };
    text.match_indices(key).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + key.len()..].chars().next();
        let left_ok = !(ascii_alnum(first) && before.is_some_and(ascii_alnum));
        let right_ok = !(ascii_alnum(last) && after.is_some_and(ascii_alnum));
        left_ok && right_ok
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    AtBot,
    Nickname(String),
    QuotedBot,
}

/// The part of the decision that needs no lookup: an @ or a nickname in the typed text. A
/// quote of the bot's own line is decided by the caller, which has to ask the archive.
pub fn decide(at_bot: bool, typed: &str, nicknames: &Nicknames) -> Option<Why> {
    if at_bot {
        return Some(Why::AtBot);
    }
    nicknames
        .hit(typed)
        .map(|nick| Why::Nickname(nick.to_owned()))
}
