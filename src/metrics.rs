//! Prometheus text endpoint on its own thread; it only reads the counters and never touches the pipeline.

use crate::exporter::Counters;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

/// Largest request head that is read; anything longer is answered as not found.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// Total time a client gets to send its request line.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// The metrics in the Prometheus text exposition format.
pub fn render(c: &Counters) -> String {
    let mut out = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, samples: &[(&str, u64)]| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for (labels, value) in samples {
            out.push_str(&format!("{name}{labels} {value}\n"));
        }
    };
    metric(
        "varnish_tracer_spans_exported_total",
        "counter",
        "Spans accepted by the receiver.",
        &[("", c.exported.load(Relaxed))],
    );
    metric(
        "varnish_tracer_spans_dropped_total",
        "counter",
        "Spans dropped, by reason: a full queue or a failed export.",
        &[
            ("{reason=\"queue\"}", c.dropped_queue.load(Relaxed)),
            ("{reason=\"export\"}", c.dropped_export.load(Relaxed)),
        ],
    );
    metric(
        "varnish_tracer_lines_malformed_total",
        "counter",
        "varnishncsa lines that were too long or not a valid record.",
        &[("", c.malformed.load(Relaxed))],
    );
    metric(
        "varnish_tracer_vsl_overruns_total",
        "counter",
        "Shared log overruns reported by varnishncsa.",
        &[("", c.vsl_overruns.load(Relaxed))],
    );
    metric(
        "varnish_tracer_queue_spans",
        "gauge",
        "Spans waiting in the export queue.",
        &[("", c.queued.load(Relaxed))],
    );
    metric(
        "varnish_tracer_up",
        "gauge",
        "Always 1 while the process runs.",
        &[("", 1)],
    );
    out
}

/// The path of a `GET` request line, `None` for any other method or a malformed line.
fn request_path(head: &[u8]) -> Option<&str> {
    let line = head.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end();
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    (method == "GET" && version.starts_with("HTTP/") && parts.next().is_none()).then_some(target)
}

fn response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Reads up to the end of the request head within the deadline; `None` when the client is too slow or too verbose.
fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(2).any(|w| w == b"\n\n") && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let left = deadline.checked_duration_since(Instant::now())?;
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
        if head.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }
    Some(head)
}

fn handle(mut stream: TcpStream, counters: &Counters) {
    let _ = stream.set_write_timeout(Some(REQUEST_DEADLINE));
    let Some(head) = read_head(&mut stream) else {
        let _ = stream.write_all(&response("404 Not Found", "text/plain", "not found\n"));
        return;
    };
    let reply = match request_path(&head) {
        Some("/metrics") => response(
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            &render(counters),
        ),
        _ => response("404 Not Found", "text/plain", "not found\n"),
    };
    let _ = stream.write_all(&reply);
}

/// Serves requests one at a time on a detached thread.
pub fn spawn(listener: TcpListener, counters: Arc<Counters>) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle(stream, &counters);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_format() {
        let counters = Counters::default();
        counters.exported.store(5, Relaxed);
        counters.dropped_queue.store(2, Relaxed);
        counters.dropped_export.store(3, Relaxed);
        counters.malformed.store(4, Relaxed);
        counters.vsl_overruns.store(1, Relaxed);
        counters.queued.store(7, Relaxed);
        let text = render(&counters);
        for expected in [
            "# TYPE varnish_tracer_spans_exported_total counter\nvarnish_tracer_spans_exported_total 5\n",
            "varnish_tracer_spans_dropped_total{reason=\"queue\"} 2\n",
            "varnish_tracer_spans_dropped_total{reason=\"export\"} 3\n",
            "varnish_tracer_lines_malformed_total 4\n",
            "varnish_tracer_vsl_overruns_total 1\n",
            "# TYPE varnish_tracer_queue_spans gauge\nvarnish_tracer_queue_spans 7\n",
            "varnish_tracer_up 1\n",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        assert!(text.ends_with('\n'));
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert_eq!(line.split(' ').count(), 2, "{line}");
        }
    }

    #[test]
    fn request_line_parsing() {
        assert_eq!(
            request_path(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/metrics")
        );
        assert_eq!(request_path(b"GET /other HTTP/1.0\n\n"), Some("/other"));
        assert_eq!(request_path(b"POST /metrics HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_path(b"GET /metrics\r\n\r\n"), None);
        assert_eq!(request_path(b"GET /metrics HTTP/1.1 extra\r\n\r\n"), None);
        assert_eq!(request_path(b"\xff\xfe\r\n\r\n"), None);
        assert_eq!(request_path(b""), None);
    }
}
