//! The shared secret that prefixes every VCL log record, so client data can never forge one.

use std::fmt;
use std::io::Read;
use std::path::Path;

const KEY_LEN: usize = 32;

/// 32 lowercase hex characters; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Key(String);

impl Key {
    /// Accepts the key with optional trailing whitespace, such as the newline of an editor or `echo`.
    pub fn parse(text: &str) -> Result<Key, String> {
        let key = text.trim_end();
        let valid =
            key.len() == KEY_LEN && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Key(key.to_owned()))
        } else {
            Err(format!("expected {KEY_LEN} lowercase hex characters"))
        }
    }

    /// Reads and validates the key file; errors never repeat its content.
    pub fn load(path: &Path) -> Result<Key, String> {
        let shown = path.display();
        let mut text = String::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(1024).read_to_string(&mut text))
            .map_err(|err| format!("key file {shown}: {err}"))?;
        Key::parse(&text).map_err(|err| format!("key file {shown}: {err}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn valid_keys() {
        assert_eq!(Key::parse(KEY).unwrap().as_str(), KEY);
        assert_eq!(Key::parse(&format!("{KEY}\n")).unwrap().as_str(), KEY);
        assert_eq!(Key::parse(&format!("{KEY} \r\n\n")).unwrap().as_str(), KEY);
    }

    #[test]
    fn invalid_keys() {
        for bad in [
            "",
            "\n",
            "0123",
            &format!("{KEY}0"),
            &KEY[1..],
            &KEY.to_uppercase(),
            &format!(" {KEY}"),
            &format!("{}g", &KEY[1..]),
            &format!("{}\n{}", &KEY[..16], &KEY[16..]),
            "0123456789abcdef0123456789abcde\u{e9}",
        ] {
            assert!(Key::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn errors_and_debug_never_show_the_key() {
        let err = Key::parse(&format!("{KEY}zz")).unwrap_err();
        assert!(!err.contains(KEY));
        assert!(!format!("{:?}", Key::parse(KEY).unwrap()).contains(KEY));
    }

    #[test]
    fn load_reads_and_validates_the_file() {
        let dir = crate::text::test_dir();
        let good = dir.join("key-good.txt");
        std::fs::write(&good, format!("{KEY}\n")).unwrap();
        assert_eq!(Key::load(&good).unwrap().as_str(), KEY);
        let bad = dir.join("key-bad.txt");
        std::fs::write(&bad, "secret-not-hex\n").unwrap();
        let err = Key::load(&bad).unwrap_err();
        assert!(err.contains("key-bad.txt") && !err.contains("secret-not-hex"));
        assert!(Key::load(&dir.join("key-missing.txt")).is_err());
        let big = dir.join("key-big.txt");
        std::fs::write(&big, "a".repeat(5000)).unwrap();
        assert!(Key::load(&big).is_err());
        std::fs::write(&big, [0xff, 0xfe]).unwrap();
        assert!(Key::load(&big).is_err());
    }
}
