//! The varnishncsa invocation: client and backend transactions, selected by the log key and printed as JSON.

use crate::exporter::Counters;
use crate::key::Key;
use crate::text::escape_log;
use std::io::{self, BufRead};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;

/// Longest accepted line, newline included.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Field names are the contract with `record::parse`; `@` stands for the key.
const FORMAT_TEMPLATE: &str = concat!(
    r#"{"side":"%{Varnish:side}x","tp":"%{VCL_Log:@_tp}x","parent":"%{VCL_Log:@_parent}x","#,
    r#""betp":"%{VCL_Log:@_be_tp}x","beparent":"%{VCL_Log:@_be_parent}x","#,
    r#""hits":"%{VCL_Log:@_hits}x","ttl":"%{VCL_Log:@_ttl}x","reason":"%{VCL_Log:@_reason}x","#,
    r#""start":"%{VSL:Timestamp:Start[1]}x","resp":"%{VSL:Timestamp:Resp[1]}x","#,
    r#""bend":"%{VSL:Timestamp:BerespBody[1]}x","bresp":"%{VSL:Timestamp:Beresp[1]}x","#,
    r#""berr":"%{VSL:Timestamp:Error[1]}x","backend":"%{VSL:BackendOpen[2]}x","fetcherr":"%{VSL:FetchError}x","#,
    r#""dur":"%D","ttfb":"%{Varnish:time_firstbyte}x","method":"%m","host":"%{Host}i","#,
    r#""path":"%U","query":"%q","proto":"%H","status":"%s","bytes":"%b","#,
    r#""handling":"%{Varnish:handling}x","vxid":"%{Varnish:vxid}x","ua":"%{User-agent}i"}"#
);

/// The JSON format with the key as prefix of every `VCL_Log` field.
pub fn format(key: &Key) -> String {
    FORMAT_TEMPLATE.replace('@', key.as_str())
}

/// Selects the transactions that carry a Varnish span or a backend span record.
pub fn query(key: &Key) -> String {
    let key = key.as_str();
    format!("VCL_Log:{key}_tp or VCL_Log:{key}_be_tp")
}

pub fn spawn(binary: &str, instance: &str, key: &Key) -> io::Result<Child> {
    Command::new(binary)
        .args(["-n", instance, "-c", "-b", "-j", "-t", "off"])
        .args(["-q", &query(key)])
        .args(["-F", &format(key)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

pub enum Line {
    Eof,
    Complete,
    TooLong,
}

/// Reads one line into `buf`; a line over `MAX_LINE_BYTES` is drained up to its newline without being stored.
pub fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Line> {
    buf.clear();
    let (mut seen, mut too_long) = (false, false);
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if chunk.is_empty() {
            return Ok(match (seen, too_long) {
                (false, _) => Line::Eof,
                (true, true) => Line::TooLong,
                (true, false) => Line::Complete,
            });
        }
        seen = true;
        let newline = chunk.iter().position(|&b| b == b'\n');
        let take = newline.map_or(chunk.len(), |i| i + 1);
        if !too_long {
            if buf.len() + take > MAX_LINE_BYTES {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(if too_long {
                Line::TooLong
            } else {
                Line::Complete
            });
        }
    }
}

/// Copies the `varnishncsa` stderr to ours and counts VSL overruns, which are silent data loss otherwise.
pub fn forward_stderr(mut stderr: impl BufRead, counters: &Counters) {
    let mut line = Vec::new();
    while let Ok(kind) = read_line(&mut stderr, &mut line) {
        match kind {
            Line::Eof => return,
            Line::TooLong => eprintln!("varnishncsa: <line too long>"),
            Line::Complete => {
                let text = String::from_utf8_lossy(&line);
                if text.contains("overrun") {
                    counters.vsl_overruns.fetch_add(1, Relaxed);
                }
                eprintln!("varnishncsa: {}", escape_log(&text));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8]) -> Vec<(bool, usize)> {
        let mut reader = input;
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            match read_line(&mut reader, &mut buf).unwrap() {
                Line::Eof => return out,
                Line::Complete => out.push((true, buf.len())),
                Line::TooLong => out.push((false, buf.len())),
            }
        }
    }

    #[test]
    fn long_lines_are_drained_not_buffered() {
        let mut input = b"ab\n".to_vec();
        input.extend(vec![b'x'; MAX_LINE_BYTES + 10]);
        input.extend(b"\ncd\nlast");
        assert_eq!(lines(&input), [(true, 3), (false, 0), (true, 3), (true, 4)]);
    }

    #[test]
    fn line_at_the_cap_is_kept() {
        let mut input = vec![b'x'; MAX_LINE_BYTES - 1];
        input.push(b'\n');
        input.extend(vec![b'x'; MAX_LINE_BYTES + 1]);
        assert_eq!(lines(&input), [(true, MAX_LINE_BYTES), (false, 0)]);
    }

    #[test]
    fn overruns_are_counted() {
        let counters = Counters::default();
        forward_stderr(
            &b"Log overrun\nsomething else\nLog overrun\n"[..],
            &counters,
        );
        assert_eq!(counters.vsl_overruns.load(Relaxed), 2);
    }
    fn key() -> Key {
        Key::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    #[test]
    fn format_is_valid_json_shape() {
        let sample = format(&key()).replace("%{VCL_Log:", "").replace('%', "");
        assert!(serde_json::from_str::<serde_json::Value>(&sample).is_ok());
    }

    #[test]
    fn format_prefixes_every_log_field_with_the_key() {
        let format = format(&key());
        let k = "0123456789abcdef0123456789abcdef";
        for field in [
            "tp",
            "parent",
            "be_tp",
            "be_parent",
            "hits",
            "ttl",
            "reason",
        ] {
            assert!(
                format.contains(&format!("%{{VCL_Log:{k}_{field}}}x")),
                "{field}"
            );
        }
        assert_eq!(format.matches("VCL_Log:").count(), 7);
        assert!(!format.contains('@') && !format.contains("tracer_"));
    }

    #[test]
    fn query_selects_client_and_backend_records_of_this_key_only() {
        assert_eq!(
            query(&key()),
            "VCL_Log:0123456789abcdef0123456789abcdef_tp or VCL_Log:0123456789abcdef0123456789abcdef_be_tp"
        );
    }

    #[test]
    fn format_carries_the_side_and_backend_fields() {
        let format = format(&key());
        for needle in [
            "%{Varnish:side}x",
            "%{VSL:Timestamp:BerespBody[1]}x",
            "%{VSL:Timestamp:Beresp[1]}x",
            "%{VSL:Timestamp:Error[1]}x",
            "%{VSL:BackendOpen[2]}x",
            "%{VSL:FetchError}x",
        ] {
            assert!(format.contains(needle), "{needle}");
        }
    }
}
