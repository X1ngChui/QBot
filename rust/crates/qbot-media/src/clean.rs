use qbot_core::marker::neutralize;

/// Model- or recogniser-written text made fit to sit inside a marker: one line, no Markdown
/// emphasis, no ASCII brackets. A picture can carry printed text, so a description is a channel
/// for outside bytes and gets the same treatment as anything a member types.
pub fn clean_description(raw: &str) -> String {
    let no_markdown: String = raw
        .lines()
        .map(|line| {
            line.trim_start_matches(['#', '>'])
                .trim_start_matches("- ")
                .trim_start_matches("* ")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['*', '`'], "")
        .replace("__", "");
    let single_line = no_markdown.split_whitespace().collect::<Vec<_>>().join(" ");
    neutralize(&single_line)
}

/// A picture's format from its first bytes.
pub fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"\xff\xd8") {
        "image/jpeg"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}
