//! Argument parsing shared by commands. Nothing here knows a command; every failure is the
//! `Msg` to reply with.

use std::time::Duration;

use qbot_i18n::Msg;

/// Split off a single `--linked` flag (include the accounts linked to the same person), wherever
/// it appears. Any other `--option` is an error.
pub fn scope<'a>(tokens: &[&'a str]) -> Result<(bool, Vec<&'a str>), Msg> {
    let linked = tokens.iter().filter(|t| **t == "--linked").count();
    if linked > 1 {
        return Err(Msg::LinkedOnce {});
    }
    if tokens
        .iter()
        .any(|t| t.starts_with("--") && **t != *"--linked")
    {
        return Err(Msg::UnknownOption {});
    }
    Ok((
        linked == 1,
        tokens
            .iter()
            .copied()
            .filter(|t| *t != "--linked")
            .collect(),
    ))
}

/// A page number or count: ASCII digits only, 1 to 10000.
pub fn positive(text: &str) -> Result<u32, Msg> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Msg::BadNumber {});
    }
    match text.parse::<u32>() {
        Ok(n) if (1..=10_000).contains(&n) => Ok(n),
        _ => Err(Msg::BadNumber {}),
    }
}

/// A duration such as `30m`, `12h` or `3d` (`humantime` syntax). Zero is not a duration.
pub fn duration(text: &str) -> Option<Duration> {
    humantime::parse_duration(text.trim())
        .ok()
        .filter(|d| !d.is_zero())
}

/// Split at the first standalone `--`: the header before it and the content after it, both
/// trimmed. The content keeps its inner whitespace.
pub fn body(text: &str) -> (&str, Option<&str>) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(found) = text[from..].find("--") {
        let at = from + found;
        let before_ok = at == 0 || bytes[at - 1].is_ascii_whitespace();
        let after_ok = at + 2 == text.len() || bytes[at + 2].is_ascii_whitespace();
        if before_ok && after_ok {
            return (text[..at].trim(), Some(text[at + 2..].trim()));
        }
        from = at + 2;
    }
    (text.trim(), None)
}

/// Keep at most `limit` characters, marking the cut.
pub fn fit(text: &str, limit: usize, marker: &str) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let keep = limit.saturating_sub(marker.chars().count() + 1);
    let head: String = text.chars().take(keep).collect();
    format!("{head}\n{marker}")
}

/// `text` truncated to `limit` characters with an ellipsis.
pub fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        let head: String = text.chars().take(limit).collect();
        format!("{head}...")
    }
}

/// What follows the first `skip` whitespace-separated words of `text`, trimmed, with its inner
/// spacing kept: free text after a command's fixed arguments.
pub fn after_words(text: &str, skip: usize) -> &str {
    let mut rest = text.trim_start();
    for _ in 0..skip {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = rest[end..].trim_start();
    }
    rest.trim_end()
}
