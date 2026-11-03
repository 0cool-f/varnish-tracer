//! Bounds and escaping for text that comes from clients or receivers.

/// Longest string attribute value, in bytes.
pub const MAX_VALUE_BYTES: usize = 1024;
const MAX_LOG_CHARS: usize = 256;

/// Cuts `text` to at most `max` bytes without splitting a UTF-8 sequence.
pub fn truncate(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Prepares untrusted text for a log line: at most 256 characters, control characters escaped.
pub fn escape_log(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars().take(MAX_LOG_CHARS) {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Directory for files that unit tests create.
#[cfg(test)]
pub fn test_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_utf8_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("aéb", 2), "a");
        assert_eq!(truncate("aéb", 3), "aé");
        assert_eq!(truncate("€", 2), "");
    }

    #[test]
    fn escape_log_caps_and_escapes() {
        assert_eq!(escape_log("a\nb\x1b[0m\r"), "a\\nb\\u{1b}[0m");
        assert_eq!(escape_log(&"x".repeat(1000)).len(), 256);
        assert_eq!(escape_log("è"), "è");
    }
}
