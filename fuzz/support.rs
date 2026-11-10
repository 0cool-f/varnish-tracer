//! Invariant checks shared by the parser fuzz targets; included at each target's root so `crate::record` resolves.

use crate::record::{self, Side};
use crate::span;
use prost::Message;

/// Parses `line` and, when it is accepted, asserts the record invariants and encodes both span variants.
pub fn check_line(line: &[u8]) {
    let Some(rec) = record::parse(line) else {
        return;
    };
    assert!(rec.start_ns != 0, "start_ns is zero: {rec:?}");
    assert!(rec.end_ns >= rec.start_ns, "end before start: {rec:?}");
    if rec.side == Side::Backend {
        assert!(rec.parent_id.is_some(), "backend without parent: {rec:?}");
    }
    for include_query in [false, true] {
        let span = span::to_span(&rec, include_query);
        let encoded = span.encode_to_vec();
        assert_eq!(encoded.len(), span.encoded_len());
    }
}
