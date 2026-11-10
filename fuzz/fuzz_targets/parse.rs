#![no_main]
#![allow(dead_code)]

#[path = "../../src/otlp.rs"]
mod otlp;
#[path = "../../src/record.rs"]
mod record;
#[path = "../../src/span.rs"]
mod span;
#[path = "../support.rs"]
mod support;
#[path = "../../src/text.rs"]
mod text;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    support::check_line(data);
});
