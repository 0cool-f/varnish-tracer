#![no_main]
#![allow(dead_code)]

#[path = "../../src/key.rs"]
mod key;
#[path = "../../src/ncsa.rs"]
mod ncsa;
#[path = "../../src/text.rs"]
mod text;

/// Stand-in for the exporter module, which `ncsa` only uses for the `vsl_overruns` counter.
mod exporter {
    use std::sync::atomic::AtomicU64;

    pub struct Counters {
        pub vsl_overruns: AtomicU64,
    }
}

use libfuzzer_sys::fuzz_target;
use ncsa::{Line, MAX_LINE_BYTES, read_line};
use std::io::{BufReader, Cursor};

// The first byte picks the reader's buffer capacity so chunk boundaries move; the rest is the stream, compared with a split on newlines.
fuzz_target!(|data: &[u8]| {
    let Some((&cap, stream)) = data.split_first() else {
        return;
    };
    let mut reader = BufReader::with_capacity(usize::from(cap) + 1, Cursor::new(stream));
    let mut buf = Vec::new();
    for expected in stream.split_inclusive(|&b| b == b'\n') {
        match read_line(&mut reader, &mut buf).unwrap() {
            Line::Complete => {
                assert!(expected.len() <= MAX_LINE_BYTES);
                assert_eq!(buf, expected);
            }
            Line::TooLong => {
                assert!(expected.len() > MAX_LINE_BYTES);
                assert!(buf.is_empty());
            }
            Line::Eof => panic!("early eof"),
        }
    }
    assert!(matches!(
        read_line(&mut reader, &mut buf).unwrap(),
        Line::Eof
    ));
});
