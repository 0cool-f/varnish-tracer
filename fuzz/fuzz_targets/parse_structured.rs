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

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

/// One string value of the JSON object, biased towards shapes the parser accepts.
#[derive(Arbitrary, Debug)]
enum Val {
    Empty,
    Text(String),
    Int(i64),
    Float(u64),
    Timestamp(u64, u32),
    Traceparent([u8; 16], [u8; 8], u8),
    SpanId([u8; 8]),
}

impl Val {
    fn render(&self) -> String {
        match self {
            Val::Empty => String::new(),
            Val::Text(text) => text.clone(),
            Val::Int(n) => n.to_string(),
            Val::Float(bits) => f64::from_bits(*bits).to_string(),
            Val::Timestamp(secs, micros) => format!("{secs}.{:06}", micros % 1_000_000),
            Val::Traceparent(trace, span, flags) => {
                format!("00-{}-{}-{flags:02x}", hex(trace), hex(span))
            }
            Val::SpanId(span) => hex(span),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Arbitrary, Debug)]
struct Input {
    backend: bool,
    fields: Vec<(u8, Val)>,
}

const NAMES: [&str; 28] = [
    "side", "tp", "parent", "betp", "beparent", "hits", "ttl", "reason", "bend", "bresp", "berr",
    "backend", "fetcherr", "start", "resp", "dur", "ttfb", "method", "host", "path", "query",
    "proto", "status", "bytes", "handling", "vxid", "ua", "late",
];

fuzz_target!(|input: Input| {
    let mut object = Map::new();
    if input.backend {
        object.insert("side".into(), Value::String("b".into()));
    }
    for (name, value) in &input.fields {
        let name = NAMES[usize::from(*name) % NAMES.len()];
        if name == "side" && input.backend {
            continue;
        }
        object.insert(name.into(), Value::String(value.render()));
    }
    let line = serde_json::to_vec(&Value::Object(object)).unwrap();
    support::check_line(&line);
});
