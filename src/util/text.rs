//! Text formatting helpers used when returning tool output to the model.

/// Truncate from the middle, keeping the head and tail (most useful for logs).
pub fn truncate_middle(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes < 64 {
        return truncate_tail(input, max_bytes);
    }

    let omitted = input.len().saturating_sub(max_bytes);
    let marker = format!("\n... [{omitted} bytes omitted] ...\n");
    let budget = max_bytes.saturating_sub(marker.len());
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;

    let head_end = floor_boundary(input, head_budget);
    let tail_start = ceil_boundary(input, input.len() - tail_budget);
    format!("{}{}{}", &input[..head_end], marker, &input[tail_start..])
}

/// Truncate the end, keeping the beginning.
pub fn truncate_tail(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes < 32 {
        return input.chars().take(max_bytes).collect();
    }
    let omitted = input.len() - max_bytes;
    let end = floor_boundary(input, max_bytes);
    let kept = &input[..end];
    format!("{kept}\n... [{omitted} bytes omitted]")
}

/// Collapse a multi-line string into a short single line, for logs.
pub fn one_line(input: &str, max_chars: usize) -> String {
    let flat: String = input
        .chars()
        .map(|c| match c {
            '\n' => '\u{23ce}', // return symbol
            '\r' => ' ',
            '\t' => ' ',
            c => c,
        })
        .collect();
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let truncated: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn floor_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(truncate_middle("hello", 100), "hello");
    }

    #[test]
    fn middle_truncation_is_bounded() {
        let text = "a".repeat(10_000);
        let out = truncate_middle(&text, 500);
        assert!(out.len() < 600, "got {} bytes", out.len());
        assert!(out.contains("bytes omitted"));
    }

    #[test]
    fn one_line_replaces_newlines() {
        assert_eq!(one_line("a\nb\tc", 100), "a⏎b c");
    }
}
