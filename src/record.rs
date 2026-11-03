//! Parsing of one `varnishncsa` JSON line into a [`Record`].

use serde::Deserialize;

/// Raw JSON object as printed by the varnishncsa format; every value is a string.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    side: String,
    tp: String,
    parent: String,
    betp: String,
    beparent: String,
    hits: String,
    ttl: String,
    reason: String,
    bend: String,
    bresp: String,
    berr: String,
    backend: String,
    fetcherr: String,
    start: String,
    resp: String,
    dur: String,
    ttfb: String,
    method: String,
    host: String,
    path: String,
    query: String,
    proto: String,
    status: String,
    bytes: String,
    handling: String,
    vxid: String,
    ua: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// The client transaction, exported as a SERVER span.
    #[default]
    Client,
    /// A backend fetch, exported as a CLIENT span.
    Backend,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record {
    pub side: Side,
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_id: Option<[u8; 8]>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub ttfb: Option<f64>,
    pub method: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub query: Option<String>,
    pub proto: Option<String>,
    pub status: Option<i64>,
    pub bytes: Option<i64>,
    pub handling: Option<String>,
    pub vxid: Option<i64>,
    pub ua: Option<String>,
    pub backend: Option<String>,
    pub fetch_error: bool,
    pub hits: Option<i64>,
    pub ttl: Option<f64>,
    pub reason: Option<String>,
}

/// Parses one line; `None` means the line is malformed.
/// A backend line (`side` is `b`) needs the Varnish span as its parent, since it is a CLIENT span below it.
pub fn parse(line: &[u8]) -> Option<Record> {
    let raw: Raw = serde_json::from_str(&String::from_utf8_lossy(line)).ok()?;
    let side = if raw.side == "b" {
        Side::Backend
    } else {
        Side::Client
    };
    let (tp, parent, ends) = match side {
        Side::Client => (&raw.tp, &raw.parent, [&raw.resp, &raw.resp, &raw.resp]),
        Side::Backend => (&raw.betp, &raw.beparent, [&raw.bend, &raw.berr, &raw.bresp]),
    };
    let (trace_id, span_id) = parse_traceparent(tp)?;
    let parent_id = present(parent).and_then(parse_span_id);
    if side == Side::Backend && parent_id.is_none() {
        return None;
    }
    let start_ns = parse_timestamp(&raw.start).filter(|&ns| ns != 0)?;
    let end_ns = ends
        .into_iter()
        .find_map(|end| present(end).and_then(parse_timestamp))
        .or_else(|| {
            let dur: u64 = present(&raw.dur)?.parse().ok()?;
            start_ns.checked_add(dur.checked_mul(1000)?)
        })
        .unwrap_or(start_ns)
        .max(start_ns);
    let finite = |v: &str| v.parse::<f64>().ok().filter(|v| v.is_finite());
    Some(Record {
        side,
        trace_id,
        span_id,
        parent_id,
        start_ns,
        end_ns,
        ttfb: present(&raw.ttfb).and_then(finite),
        method: owned(&raw.method),
        host: owned(&raw.host),
        path: owned(&raw.path),
        query: present(&raw.query)
            .map(|q| q.strip_prefix('?').unwrap_or(q))
            .filter(|q| !q.is_empty())
            .map(str::to_owned),
        proto: owned(&raw.proto),
        status: present(&raw.status).and_then(|v| v.parse().ok()),
        // CLF prints "-" for a response without a body
        bytes: match raw.bytes.as_str() {
            "-" => Some(0),
            v => v.parse().ok(),
        },
        handling: owned(&raw.handling),
        vxid: present(&raw.vxid).and_then(|v| v.parse().ok()),
        ua: owned(&raw.ua),
        backend: owned(&raw.backend),
        fetch_error: side == Side::Backend
            && (present(&raw.fetcherr).is_some() || present(&raw.berr).is_some()),
        hits: present(&raw.hits).and_then(|v| v.parse().ok()),
        ttl: present(&raw.ttl).and_then(finite),
        reason: owned(&raw.reason),
    })
}

fn present(value: &str) -> Option<&str> {
    match value {
        "" | "-" => None,
        v => Some(v),
    }
}

fn owned(value: &str) -> Option<String> {
    present(value).map(str::to_owned)
}

fn hex_bytes<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

fn parse_span_id(text: &str) -> Option<[u8; 8]> {
    hex_bytes::<8>(text).filter(|id| id.iter().any(|&b| b != 0))
}

fn parse_traceparent(tp: &str) -> Option<([u8; 16], [u8; 8])> {
    let mut parts = tp.split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || version != "00" {
        return None;
    }
    hex_bytes::<1>(flags)?;
    let trace_id = hex_bytes::<16>(trace).filter(|id| id.iter().any(|&b| b != 0))?;
    Some((trace_id, parse_span_id(span)?))
}

/// Converts `seconds[.fraction]` to unix nanoseconds without floating point.
fn parse_timestamp(text: &str) -> Option<u64> {
    let (secs, frac) = text.split_once('.').unwrap_or((text, ""));
    if secs.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: u64 = secs.parse().ok()?;
    let mut nanos = 0u64;
    for i in 0..9 {
        let digit = frac.as_bytes().get(i).map_or(0, |b| u64::from(b - b'0'));
        nanos = nanos * 10 + digit;
    }
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"tp":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","parent":"00f067aa0ba902b7","start":"1793455717.323367","resp":"1793455717.323545","dur":"177","ttfb":"0.000076","method":"GET","host":"ncsa-varnish:8080","path":"/a%20b/c","query":"?x=1&y=2","proto":"HTTP/1.1","status":"404","bytes":"230","handling":"synth","vxid":"2","ua":"Wget"}"#;

    fn with(key: &str, value: &str) -> String {
        let needle = format!("\"{key}\":\"");
        let from = SAMPLE.find(&needle).unwrap() + needle.len();
        let to = from + SAMPLE[from..].find('"').unwrap();
        format!("{}{}{}", &SAMPLE[..from], value, &SAMPLE[to..])
    }

    #[test]
    fn sample_line() {
        let r = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(r.trace_id[0], 0x0a);
        assert_eq!(r.span_id, [0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]);
        assert_eq!(r.parent_id.unwrap()[0], 0x00);
        assert_eq!(r.parent_id.unwrap()[1], 0xf0);
        assert_eq!(r.start_ns, 1_793_455_717_323_367_000);
        assert_eq!(r.end_ns, 1_793_455_717_323_545_000);
        assert_eq!(r.ttfb, Some(0.000076));
        assert_eq!(r.method.as_deref(), Some("GET"));
        assert_eq!(r.host.as_deref(), Some("ncsa-varnish:8080"));
        assert_eq!(r.path.as_deref(), Some("/a%20b/c"));
        assert_eq!(r.query.as_deref(), Some("x=1&y=2"));
        assert_eq!(r.proto.as_deref(), Some("HTTP/1.1"));
        assert_eq!(r.status, Some(404));
        assert_eq!(r.bytes, Some(230));
        assert_eq!(r.handling.as_deref(), Some("synth"));
        assert_eq!(r.vxid, Some(2));
        assert_eq!(r.ua.as_deref(), Some("Wget"));
    }

    #[test]
    fn absent_values() {
        let mut line = with("parent", "-");
        line = line.replace("\"ua\":\"Wget\"", "\"ua\":\"\"");
        let r = parse(line.as_bytes()).unwrap();
        assert_eq!(r.parent_id, None);
        assert_eq!(r.ua, None);
        let r = parse(with("bytes", "").as_bytes()).unwrap();
        assert_eq!(r.bytes, None);
        let r = parse(with("query", "").as_bytes()).unwrap();
        assert_eq!(r.query, None);
    }

    #[test]
    fn malformed_lines() {
        assert!(parse(b"not json").is_none());
        assert!(parse(b"{}").is_none());
        assert!(parse(with("tp", "garbage").as_bytes()).is_none());
        assert!(parse(with("tp", "-").as_bytes()).is_none());
        assert!(parse(with("start", "-").as_bytes()).is_none());
        let zero_trace = format!("00-{}-b7ad6b7169203331-01", "0".repeat(32));
        assert!(parse(with("tp", &zero_trace).as_bytes()).is_none());
        let zero_span = format!("00-{}-{}-01", "a".repeat(32), "0".repeat(16));
        assert!(parse(with("tp", &zero_span).as_bytes()).is_none());
        let bad_version = format!("01-{}-{}-01", "a".repeat(32), "b".repeat(16));
        assert!(parse(with("tp", &bad_version).as_bytes()).is_none());
    }

    #[test]
    fn dash_bytes_means_zero() {
        let r = parse(with("bytes", "-").as_bytes()).unwrap();
        assert_eq!(r.bytes, Some(0));
    }

    #[test]
    fn zero_start_is_rejected() {
        assert!(parse(with("start", "0.000000").as_bytes()).is_none());
        assert!(parse(with("start", "0").as_bytes()).is_none());
    }

    #[test]
    fn invalid_utf8_becomes_replacement_character() {
        let mut line = with("path", "/a_b").into_bytes();
        let at = line.windows(4).position(|w| w == b"/a_b").unwrap() + 2;
        line[at] = 0xff;
        let r = parse(&line).unwrap();
        assert_eq!(r.path.as_deref(), Some("/a\u{fffd}b"));
    }

    #[test]
    fn invalid_parent_is_ignored() {
        for p in ["0000000000000000", "xyz", "00f067aa"] {
            assert_eq!(parse(with("parent", p).as_bytes()).unwrap().parent_id, None);
        }
    }

    #[test]
    fn end_falls_back_to_duration() {
        let r = parse(with("resp", "-").as_bytes()).unwrap();
        assert_eq!(r.end_ns, 1_793_455_717_323_367_000 + 177_000);
    }

    #[test]
    fn timestamp_precision() {
        assert_eq!(
            parse_timestamp("1793455717.323367"),
            Some(1_793_455_717_323_367_000)
        );
        assert_eq!(
            parse_timestamp("1793455717.1"),
            Some(1_793_455_717_100_000_000)
        );
        assert_eq!(
            parse_timestamp("1793455717.123456789123"),
            Some(1_793_455_717_123_456_789)
        );
        assert_eq!(
            parse_timestamp("1793455717"),
            Some(1_793_455_717_000_000_000)
        );
        assert_eq!(parse_timestamp("1.x"), None);
        assert_eq!(parse_timestamp(".5"), None);
    }

    const BACKEND: &str = r#"{"side":"b","tp":"","parent":"","betp":"00-0af7651916cd43dd8448eb211c80319c-1111111111111111-01","beparent":"b7ad6b7169203331","hits":"","ttl":"","reason":"","bend":"1793455717.323999","bresp":"1793455717.323900","berr":"","backend":"default","fetcherr":"","start":"1793455717.323400","resp":"","dur":"599","ttfb":"0.000500","method":"GET","host":"origin","path":"/a","query":"","proto":"HTTP/1.1","status":"200","bytes":"829","handling":"-","vxid":"3","ua":"curl"}"#;

    /// Replaces the value of `key` in a line that has it.
    fn set(line: &str, key: &str, value: &str) -> String {
        let needle = format!("\"{key}\":\"");
        let from = line.find(&needle).unwrap() + needle.len();
        let to = from + line[from..].find('"').unwrap();
        format!("{}{}{}", &line[..from], value, &line[to..])
    }

    fn backend(changes: &[(&str, &str)]) -> Vec<u8> {
        let line = changes
            .iter()
            .fold(BACKEND.to_owned(), |l, (k, v)| set(&l, k, v));
        line.into_bytes()
    }

    #[test]
    fn backend_line() {
        let r = parse(BACKEND.as_bytes()).unwrap();
        assert_eq!(r.side, Side::Backend);
        assert_eq!(r.trace_id[0], 0x0a);
        assert_eq!(r.span_id, [0x11; 8]);
        assert_eq!(r.parent_id.unwrap()[0], 0xb7);
        assert_eq!(r.start_ns, 1_793_455_717_323_400_000);
        assert_eq!(r.end_ns, 1_793_455_717_323_999_000);
        assert_eq!(r.backend.as_deref(), Some("default"));
        assert_eq!(r.method.as_deref(), Some("GET"));
        assert_eq!(r.path.as_deref(), Some("/a"));
        assert_eq!(r.status, Some(200));
        assert_eq!(r.ttfb, Some(0.0005));
        assert_eq!(r.vxid, Some(3));
        assert!(!r.fetch_error);
    }

    #[test]
    fn backend_end_prefers_body_then_error_then_response_then_duration() {
        let end = |changes: &[(&str, &str)]| parse(&backend(changes)).unwrap().end_ns;
        assert_eq!(end(&[]), 1_793_455_717_323_999_000);
        let all = [("berr", "1793455717.324100")];
        assert_eq!(end(&all), 1_793_455_717_323_999_000);
        assert_eq!(
            end(&[("bend", ""), ("berr", "1793455717.324100")]),
            1_793_455_717_324_100_000
        );
        assert_eq!(end(&[("bend", "-")]), 1_793_455_717_323_900_000);
        assert_eq!(
            end(&[("bend", ""), ("bresp", "")]),
            1_793_455_717_323_400_000 + 599_000
        );
    }

    #[test]
    fn fetch_errors_are_detected() {
        let error = |changes: &[(&str, &str)]| parse(&backend(changes)).unwrap().fetch_error;
        assert!(!error(&[]));
        assert!(error(&[("fetcherr", "backend default: fail errno 110")]));
        assert!(error(&[("berr", "1793455717.324100")]));
    }

    #[test]
    fn backend_line_needs_its_own_fields() {
        for changes in [
            [("betp", "")],
            [("betp", "garbage")],
            [("beparent", "")],
            [("beparent", "0000000000000000")],
        ] {
            assert!(parse(&backend(&changes)).is_none(), "{changes:?}");
        }
        // A client line with only a backend traceparent is not a span
        assert!(parse(&backend(&[("side", "c")])).is_none());
    }

    #[test]
    fn client_fields_never_leak_into_a_backend_record() {
        let tp = "00-0af7651916cd43dd8448eb211c80319c-2222222222222222-01";
        assert_eq!(parse(&backend(&[("tp", tp)])).unwrap().span_id, [0x11; 8]);
    }

    #[test]
    fn cache_attributes() {
        let client = format!(
            "{}\"hits\":\"3\",\"ttl\":\"12.185\",\"reason\":\"static asset\",{}",
            &SAMPLE[..1],
            &SAMPLE[1..]
        );
        let r = parse(client.as_bytes()).unwrap();
        assert_eq!(r.hits, Some(3));
        assert_eq!(r.ttl, Some(12.185));
        assert_eq!(r.reason.as_deref(), Some("static asset"));
        let r = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!((r.hits, r.ttl, r.reason), (None, None, None));
    }

    #[test]
    fn ttl_may_be_negative_and_non_finite_numbers_are_dropped() {
        let ttl = |value: &str| {
            let line = format!("{}\"ttl\":\"{value}\",{}", &SAMPLE[..1], &SAMPLE[1..]);
            parse(line.as_bytes()).unwrap().ttl
        };
        assert_eq!(ttl("-4.5"), Some(-4.5));
        assert_eq!(ttl("NaN"), None);
        assert_eq!(ttl("inf"), None);
        assert_eq!(parse(with("ttfb", "inf").as_bytes()).unwrap().ttfb, None);
    }
}
