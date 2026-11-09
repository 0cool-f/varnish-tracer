//! Request rate, status and latency of all client requests, read by a second `varnishncsa` with a minimal format.

use crate::ncsa::{self, Line};
use std::fmt::Write as _;
use std::io::{self, BufRead};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One line per client transaction: status, handling and time to first byte in seconds.
const FORMAT: &str = "%s %{Varnish:handling}x %{Varnish:time_firstbyte}x";

const HANDLINGS: [&str; 6] = ["hit", "miss", "pass", "pipe", "synth", "other"];
const CODES: [&str; 6] = ["1xx", "2xx", "3xx", "4xx", "5xx", "other"];
/// Upper bounds in seconds of the time to first byte histogram buckets, `+Inf` excluded.
const BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Default)]
pub struct Red {
    requests: [[AtomicU64; CODES.len()]; HANDLINGS.len()],
    /// Observations per bucket, not cumulative; the last slot is above every bound.
    ttfb_buckets: [[AtomicU64; BUCKETS.len() + 1]; HANDLINGS.len()],
    ttfb_sum_us: [AtomicU64; HANDLINGS.len()],
    malformed: AtomicU64,
}

fn handling_index(handling: &str) -> usize {
    HANDLINGS[..HANDLINGS.len() - 1]
        .iter()
        .position(|&h| h == handling)
        .unwrap_or(HANDLINGS.len() - 1)
}

fn code_index(status: u16) -> usize {
    match status {
        100..=599 => usize::from(status / 100 - 1),
        _ => CODES.len() - 1,
    }
}

impl Red {
    /// Counts one line; a line without a valid status is counted as malformed and a missing time to first byte
    /// leaves the histogram untouched.
    pub fn observe(&self, line: &str) {
        let mut fields = line.split_ascii_whitespace();
        let (Some(status), Some(handling)) = (
            fields.next().and_then(|s| s.parse::<u16>().ok()),
            fields.next(),
        ) else {
            self.malformed.fetch_add(1, Relaxed);
            return;
        };
        let h = handling_index(handling);
        self.requests[h][code_index(status)].fetch_add(1, Relaxed);
        let Some(ttfb) = fields
            .next()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|t| t.is_finite() && *t >= 0.0)
        else {
            return;
        };
        let bucket = BUCKETS
            .iter()
            .position(|&bound| ttfb <= bound)
            .unwrap_or(BUCKETS.len());
        self.ttfb_buckets[h][bucket].fetch_add(1, Relaxed);
        self.ttfb_sum_us[h].fetch_add((ttfb * 1e6).round() as u64, Relaxed);
    }

    /// The metrics in the Prometheus text exposition format, appended to `out`.
    pub fn render(&self, out: &mut String) {
        let _ = write!(
            out,
            "# HELP varnish_tracer_requests_total Client requests, by handling and status class.\n\
             # TYPE varnish_tracer_requests_total counter\n"
        );
        for (h, handling) in HANDLINGS.iter().enumerate() {
            for (c, code) in CODES.iter().enumerate() {
                let value = self.requests[h][c].load(Relaxed);
                let _ = writeln!(
                    out,
                    "varnish_tracer_requests_total{{handling=\"{handling}\",code=\"{code}\"}} {value}"
                );
            }
        }
        let name = "varnish_tracer_request_ttfb_seconds";
        let _ = write!(
            out,
            "# HELP {name} Time to first byte of client requests, by handling.\n# TYPE {name} histogram\n"
        );
        for (h, handling) in HANDLINGS.iter().enumerate() {
            let mut cumulative = 0;
            for (b, slot) in self.ttfb_buckets[h].iter().enumerate() {
                cumulative += slot.load(Relaxed);
                let le = BUCKETS.get(b).map_or("+Inf".to_owned(), f64::to_string);
                let _ = writeln!(
                    out,
                    "{name}_bucket{{handling=\"{handling}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let sum = self.ttfb_sum_us[h].load(Relaxed) as f64 / 1e6;
            let _ = writeln!(out, "{name}_sum{{handling=\"{handling}\"}} {sum}");
            let _ = writeln!(out, "{name}_count{{handling=\"{handling}\"}} {cumulative}");
        }
        let _ = write!(
            out,
            "# HELP varnish_tracer_requests_malformed_total Request lines without a valid status.\n\
             # TYPE varnish_tracer_requests_malformed_total counter\n\
             varnish_tracer_requests_malformed_total {}\n",
            self.malformed.load(Relaxed)
        );
    }
}

/// Starts the `varnishncsa` that prints one line per client transaction, ESI subrequests included.
pub fn spawn(binary: &str, instance: &str) -> io::Result<Child> {
    Command::new(binary)
        .args(["-n", instance, "-c", "-t", "off", "-F", FORMAT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Counts lines until EOF or a read error.
pub fn pump(mut reader: impl BufRead, red: &Red) {
    let mut line = Vec::new();
    loop {
        match ncsa::read_line(&mut reader, &mut line) {
            Ok(Line::Complete) => red.observe(&String::from_utf8_lossy(&line)),
            Ok(Line::TooLong) => {
                red.malformed.fetch_add(1, Relaxed);
            }
            Ok(Line::Eof) => return,
            Err(err) => {
                eprintln!("varnish-tracer: request reader error: {err}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(lines: &str) -> String {
        let red = Red::default();
        pump(lines.as_bytes(), &red);
        let mut out = String::new();
        red.render(&mut out);
        out
    }

    #[test]
    fn counts_by_handling_and_status_class() {
        let text = rendered(
            "200 hit 0.000100\n200 hit 0.0002\n503 miss 0.3\n302 hitmiss 0.02\n999 pass 1\n",
        );
        for expected in [
            "varnish_tracer_requests_total{handling=\"hit\",code=\"2xx\"} 2\n",
            "varnish_tracer_requests_total{handling=\"miss\",code=\"5xx\"} 1\n",
            "varnish_tracer_requests_total{handling=\"other\",code=\"3xx\"} 1\n",
            "varnish_tracer_requests_total{handling=\"pass\",code=\"other\"} 1\n",
            "varnish_tracer_requests_total{handling=\"miss\",code=\"2xx\"} 0\n",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
    }

    #[test]
    fn histogram_is_cumulative_with_sum_and_count() {
        let text = rendered("200 miss 0.0005\n200 miss 0.3\n200 miss 20\n200 miss -\n");
        for expected in [
            "varnish_tracer_request_ttfb_seconds_bucket{handling=\"miss\",le=\"0.001\"} 1\n",
            "varnish_tracer_request_ttfb_seconds_bucket{handling=\"miss\",le=\"0.25\"} 1\n",
            "varnish_tracer_request_ttfb_seconds_bucket{handling=\"miss\",le=\"0.5\"} 2\n",
            "varnish_tracer_request_ttfb_seconds_bucket{handling=\"miss\",le=\"10\"} 2\n",
            "varnish_tracer_request_ttfb_seconds_bucket{handling=\"miss\",le=\"+Inf\"} 3\n",
            "varnish_tracer_request_ttfb_seconds_sum{handling=\"miss\"} 20.3005\n",
            "varnish_tracer_request_ttfb_seconds_count{handling=\"miss\"} 3\n",
            "varnish_tracer_requests_total{handling=\"miss\",code=\"2xx\"} 4\n",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
    }

    #[test]
    fn malformed_lines_are_counted_and_skipped() {
        let mut input = "\n- hit 0.1\nabc\n200\n".to_owned();
        input.push_str(&"x".repeat(ncsa::MAX_LINE_BYTES + 1));
        input.push_str("\n200 synth NaN\n");
        let text = rendered(&input);
        assert!(
            text.contains("varnish_tracer_requests_malformed_total 5\n"),
            "{text}"
        );
        assert!(
            text.contains("varnish_tracer_requests_total{handling=\"synth\",code=\"2xx\"} 1\n")
        );
        assert!(text.contains("varnish_tracer_request_ttfb_seconds_count{handling=\"synth\"} 0\n"));
    }

    #[test]
    fn every_sample_line_has_a_name_and_a_value() {
        let text = rendered("200 hit 0.1\n");
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert_eq!(line.split(' ').count(), 2, "{line}");
        }
    }
}
