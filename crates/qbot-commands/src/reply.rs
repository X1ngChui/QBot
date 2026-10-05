//! Splitting long replies into messages the platform accepts.

/// Cut `text` into pieces of at most `limit` characters, preferring line boundaries; a line
/// longer than the limit is cut hard. An empty text still yields one (empty) piece.
pub fn split(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for line in text.split_inclusive('\n') {
        let len = line.chars().count();
        if current_len + len > limit && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if len > limit {
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(limit) {
                pieces.push(chunk.iter().collect());
            }
        } else {
            current.push_str(line);
            current_len += len;
        }
    }
    if !current.is_empty() || pieces.is_empty() {
        pieces.push(current);
    }
    pieces
}
