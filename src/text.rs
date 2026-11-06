//! Small text helpers shared by every command.

/// Largest index `<= pos` that falls on a UTF-8 character boundary.
#[must_use]
pub fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Shorten `s` to at most `max` bytes, ending with `...` when it was cut.
///
/// Never splits a multi-byte character, so it is safe for any URL or title.
#[must_use]
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = floor_char_boundary(s, max.saturating_sub(3));
    format!("{}...", &s[..end])
}

/// Cut `s` to at most `max` bytes plus `...`, preferring a word boundary
/// within the last 50 bytes. Never splits a multi-byte character.
#[must_use]
pub fn cut_at_word(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = floor_char_boundary(s, max);
    let head = &s[..end];
    match head.rfind(' ') {
        Some(space) if space > end.saturating_sub(50) => format!("{}...", &s[..space]),
        _ => format!("{head}..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_at_word_prefers_spaces() {
        assert_eq!(cut_at_word("short", 10), "short");
        assert_eq!(cut_at_word("hello wonderful world", 12), "hello...");
        assert_eq!(
            cut_at_word(&"x".repeat(100), 10),
            format!("{}...", "x".repeat(10))
        );
        let far = format!("a {}", "b".repeat(100));
        assert_eq!(cut_at_word(&far, 80), format!("a {}...", "b".repeat(78)));
        assert_eq!(cut_at_word("ééééé", 3), "é...");
    }

    #[test]
    fn short_strings_are_unchanged() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn long_strings_end_with_ellipsis() {
        assert_eq!(truncate("this is a very long string", 10), "this is...");
    }

    #[test]
    fn multi_byte_characters_are_not_split() {
        // Each "é" is 2 bytes; a byte slice at 7 would panic.
        let url = "https://exemple.ca/é/é/é/é/é/é/é/é/é/é";
        let out = truncate(url, 25);
        assert!(out.ends_with("..."));
        assert!(out.len() <= 25);

        let dashes = "─".repeat(100);
        let out = truncate(&dashes, 20);
        assert!(out.len() <= 20);
    }

    #[test]
    fn tiny_limits_do_not_underflow() {
        assert_eq!(truncate("abcdef", 2), "...");
    }

    #[test]
    fn floor_char_boundary_handles_edges() {
        assert_eq!(floor_char_boundary("abc", 10), 3);
        assert_eq!(floor_char_boundary("é", 1), 0);
        assert_eq!(floor_char_boundary("aé", 2), 1);
    }
}
