//! Rich cards: what a shared contact, location, song, link or mini-app is about.
//!
//! QQ delivers all of these as one `json` segment holding an "ark" document (`app`, `view`,
//! `prompt`, and the card's fields under `meta`). Only a few fields matter to a reader: whose
//! card, which place, which song, which page and from where. Everything is external text, so it
//! is cleaned and bounded here, and escaped as a marker value when it is rendered.

use serde_json::{Map, Value};

/// Characters kept of one card field: a card enters every later prompt that shows its line.
const FIELD_CHARS: usize = 120;
/// A longer link is left out rather than cut, since a cut link leads nowhere.
const URL_CHARS: usize = 400;

/// A card, reduced to what a reader needs. Every field is optional: cards differ by client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Card {
    /// A recommended QQ user.
    Contact { name: Option<String> },
    /// A recommended group.
    Group { name: Option<String> },
    Location {
        name: Option<String>,
        address: Option<String>,
    },
    Music {
        title: Option<String>,
        artist: Option<String>,
        url: Option<String>,
    },
    /// A shared page, article, video or mini-app.
    Link {
        title: Option<String>,
        text: Option<String>,
        /// The app or site it came from.
        source: Option<String>,
        url: Option<String>,
    },
    /// Anything else: the client's one-line summary, when there is one.
    Other { prompt: Option<String> },
}

/// One line of external text, at most `FIELD_CHARS` characters; `None` when empty.
pub(crate) fn field(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => line(s),
        Value::Number(n) => line(&n.to_string()),
        _ => None,
    }
}

/// `text` on one line, at most `FIELD_CHARS` characters; `None` when empty.
pub(crate) fn line(text: &str) -> Option<String> {
    let line = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return None;
    }
    let mut chars = line.chars();
    let kept: String = chars.by_ref().take(FIELD_CHARS).collect();
    Some(if chars.next().is_some() {
        format!("{kept}...")
    } else {
        kept
    })
}

/// A web link worth showing: http(s) and not absurdly long.
fn url(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    ((text.starts_with("https://") || text.starts_with("http://"))
        && text.chars().count() <= URL_CHARS
        && !text.chars().any(char::is_whitespace))
    .then(|| text.to_owned())
}

/// The card in an ark document, given as its JSON text or as the parsed object.
pub fn ark(raw: Option<&Value>) -> Card {
    let parsed;
    let doc = match raw {
        Some(Value::String(text)) => {
            parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            &parsed
        }
        Some(value) => value,
        None => &Value::Null,
    };
    let app = doc.get("app").and_then(Value::as_str).unwrap_or("");
    let prompt = || field(doc.get("prompt"));
    let meta = doc.get("meta").and_then(Value::as_object);
    // The card's own fields sit in one object under `meta`, named after the card's view.
    let part = |name: &str| meta.and_then(|m| m.get(name)).and_then(Value::as_object);
    let first = || meta.and_then(|m| m.values().find_map(Value::as_object));
    let get = |fields: Option<&Map<String, Value>>, name: &str| field(fields?.get(name));
    let link = |fields: Option<&Map<String, Value>>| {
        let fields = fields?;
        ["jumpUrl", "qqdocurl", "url"]
            .iter()
            .find_map(|name| url(fields.get(*name)))
    };
    match app {
        "com.tencent.contact.lua" => Card::Contact {
            name: get(part("contact"), "nickname"),
        },
        "com.tencent.troopsharecard" => Card::Group {
            name: get(part("contact").or_else(first), "nickname"),
        },
        "com.tencent.map" => {
            let place = part("Location.Search").or_else(first);
            Card::Location {
                name: get(place, "name"),
                address: get(place, "address"),
            }
        }
        _ if part("music").is_some() || app.contains("music") => {
            let song = part("music").or_else(first);
            Card::Music {
                title: get(song, "title"),
                artist: get(song, "desc"),
                url: link(song),
            }
        }
        // A mini-app's title is the app; what was shared is its description.
        _ if part("detail_1").is_some() => {
            let detail = part("detail_1");
            Card::Link {
                title: get(detail, "desc"),
                text: None,
                source: get(detail, "title"),
                url: link(detail),
            }
        }
        _ => {
            let fields = part("news").or_else(first);
            let (title, url) = (get(fields, "title"), link(fields));
            if title.is_none() && url.is_none() {
                Card::Other { prompt: prompt() }
            } else {
                Card::Link {
                    title,
                    text: get(fields, "desc"),
                    source: get(fields, "tag"),
                    url,
                }
            }
        }
    }
}

/// The `brief` attribute of an XML rich message (its one-line summary).
pub fn xml_brief(raw: Option<&Value>) -> Card {
    let brief = raw.and_then(Value::as_str).and_then(|xml| {
        let start = xml.find("brief=\"")? + "brief=\"".len();
        let end = xml[start..].find('"')? + start;
        field(Some(&Value::String(xml[start..end].to_owned())))
    });
    Card::Other { prompt: brief }
}

/// `[kind:key=value; ...]` with the present fields, or `[kind]` when there are none. The caller
/// escapes the values (`marker_value`).
pub(crate) fn marker(kind: &str, fields: &[(&str, &Option<String>)]) -> String {
    let present: Vec<String> = fields
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|v| format!("{key}={v}")))
        .collect();
    if present.is_empty() {
        format!("[{kind}]")
    } else {
        format!("[{kind}:{}]", present.join("; "))
    }
}
