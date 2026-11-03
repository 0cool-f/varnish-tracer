//! Runs the binary against fake OTLP receivers and a fake `varnishncsa`.

use prost::Message;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LINE: &str = r#"{"tp":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01","start":"1793455717.323367","method":"GET"}"#;

#[derive(Clone, Debug)]
struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == name)
            .map(|(_, v)| v.as_str())
    }
}

struct Fake {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn url(&self) -> String {
        format!("http://{}/v1/traces", self.addr)
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn http(status: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
    for header in headers {
        out.extend(format!("{header}\r\n").bytes());
    }
    out.extend(format!("Content-Length: {}\r\n\r\n", body.len()).bytes());
    out.extend(body);
    out
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend(&chunk[..n]),
        }
    }
    let body = data[end..end + length].to_vec();
    Some(Request {
        path,
        headers,
        body,
    })
}

/// Answers the n-th request with the bytes `reply` returns for it.
fn serve(reply: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let reply = Arc::new(reply);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (seen, reply) = (seen.clone(), reply.clone());
            std::thread::spawn(move || {
                let mut stream = stream;
                let Some(request) = read_request(&mut stream) else {
                    return;
                };
                let index = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(request);
                    seen.len() - 1
                };
                let _ = stream.write_all(&reply(index));
            });
        }
    });
    Fake { addr, requests }
}

/// Accepts connections and never answers.
fn hang() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    addr
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

impl Run {
    fn counter(&self, name: &str) -> u64 {
        let report = self
            .stderr
            .lines()
            .rfind(|l| l.contains("exported="))
            .unwrap_or_else(|| panic!("no report in:\n{}", self.stderr));
        let key = format!("{name}=");
        let from = report.find(&key).unwrap() + key.len();
        report[from..].split(' ').next().unwrap().parse().unwrap()
    }
}

fn command(args: &[&str], envs: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_varnish-tracer"));
    cmd.env_clear().envs(envs.iter().copied()).args(args);
    cmd
}

fn run_stdin(args: &[&str], envs: &[(&str, &str)], input: &str) -> Run {
    let started = Instant::now();
    let mut child = command(&[&["--stdin"], args].concat(), envs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    }
}

fn lines(n: usize) -> String {
    format!("{LINE}\n").repeat(n)
}

fn response(rejected: i64, message: &str) -> Vec<u8> {
    use varnish_tracer_proto::*;
    ExportTraceServiceResponse {
        partial_success: Some(ExportTracePartialSuccess {
            rejected_spans: rejected,
            error_message: message.to_owned(),
        }),
    }
    .encode_to_vec()
}

/// Wire-compatible copies of the OTLP messages the tests read back.
mod varnish_tracer_proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExportTraceServiceRequest {
        #[prost(message, repeated, tag = "1")]
        pub resource_spans: Vec<ResourceSpans>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ResourceSpans {
        #[prost(message, repeated, tag = "2")]
        pub scope_spans: Vec<ScopeSpans>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ScopeSpans {
        #[prost(message, repeated, tag = "2")]
        pub spans: Vec<Span>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Span {
        #[prost(string, tag = "5")]
        pub name: String,
        #[prost(int32, tag = "6")]
        pub kind: i32,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExportTraceServiceResponse {
        #[prost(message, optional, tag = "1")]
        pub partial_success: Option<ExportTracePartialSuccess>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExportTracePartialSuccess {
        #[prost(int64, tag = "1")]
        pub rejected_spans: i64,
        #[prost(string, tag = "2")]
        pub error_message: String,
    }
}

#[test]
fn retry_after_replaces_the_fixed_backoff() {
    let fake = serve(|n| {
        if n == 0 {
            http("429 Too Many Requests", &["Retry-After: 0"], b"slow down")
        } else {
            http("200 OK", &[], b"")
        }
    });
    let run = run_stdin(&["--endpoint", &fake.url()], &[], &lines(1));
    assert_eq!(fake.count(), 2);
    assert_eq!(run.counter("exported"), 1);
    assert!(
        run.elapsed < Duration::from_millis(900),
        "{:?}",
        run.elapsed
    );
}

#[test]
fn partial_success_counts_rejected_spans_as_dropped() {
    let fake = serve(|_| http("200 OK", &[], &response(2, "bad\nspan\x1b[31m")));
    let run = run_stdin(&["--endpoint", &fake.url()], &[], &lines(3));
    assert_eq!(run.counter("exported"), 1);
    assert_eq!(run.counter("dropped_export"), 2);
    assert!(
        run.stderr.contains("bad\\nspan\\u{1b}[31m"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains('\x1b'));
}

#[test]
fn empty_success_body_is_full_success() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let run = run_stdin(&["--endpoint", &fake.url()], &[], &lines(3));
    assert_eq!(run.counter("exported"), 3);
    assert_eq!(run.counter("dropped_export"), 0);
}

#[test]
fn redirects_are_fatal_and_not_followed() {
    let target = serve(|_| http("200 OK", &[], b""));
    let location = format!("Location: {}", target.url());
    let fake = serve(move |_| http("307 Temporary Redirect", &[&location], b""));
    let run = run_stdin(&["--endpoint", &fake.url()], &[], &lines(1));
    assert_eq!(fake.count(), 1);
    assert_eq!(target.count(), 0);
    assert_eq!(run.counter("dropped_export"), 1);
    assert!(run.elapsed < Duration::from_millis(900));
}

#[test]
fn client_errors_are_not_retried() {
    let fake = serve(|_| http("400 Bad Request", &[], b"no\x07pe"));
    let run = run_stdin(&["--endpoint", &fake.url()], &[], &lines(1));
    assert_eq!(fake.count(), 1);
    assert_eq!(run.counter("dropped_export"), 1);
    assert!(run.stderr.contains("no\\u{7}pe"));
}

#[test]
fn environment_proxies_are_ignored() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let fake = serve(|_| http("200 OK", &[], b""));
    let envs = [
        ("HTTP_PROXY", proxy_url.as_str()),
        ("http_proxy", proxy_url.as_str()),
        ("ALL_PROXY", proxy_url.as_str()),
    ];
    let run = run_stdin(&["--endpoint", &fake.url()], &envs, &lines(1));
    assert_eq!(run.counter("exported"), 1);
    assert_eq!(fake.count(), 1);
    assert!(proxy.accept().is_err(), "the proxy was contacted");
}

#[test]
fn endpoint_and_headers_come_from_the_environment_and_flags() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let base = format!("http://{}/", fake.addr);
    let envs = [
        ("OTEL_EXPORTER_OTLP_ENDPOINT", base.as_str()),
        ("OTEL_EXPORTER_OTLP_HEADERS", "X-Env=1,X-Auth=Api-Token%20t"),
    ];
    let run = run_stdin(&["--header", "X-Flag=a,b"], &envs, &lines(1));
    assert_eq!(run.counter("exported"), 1);
    let request = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(request.path, "/v1/traces");
    assert_eq!(request.header("x-env"), Some("1"));
    assert_eq!(request.header("x-auth"), Some("Api-Token t"));
    assert_eq!(request.header("x-flag"), Some("a,b"));
}

#[test]
fn invalid_configuration_exits_with_2() {
    for (args, envs) in [
        (vec!["--endpoint", "ftp://example.com/x"], vec![]),
        (vec!["--endpoint", "localhost:4318"], vec![]),
        (vec!["--header", "no-equals"], vec![]),
        (vec!["--header", "bad name=x"], vec![]),
        (vec!["--header", "k=a%0d%0ab"], vec![]),
        (vec![], vec![("OTEL_EXPORTER_OTLP_HEADERS", "A=1,broken")]),
        (vec!["--queue-size", "0"], vec![]),
    ] {
        let run = run_stdin(&args, &envs, "");
        assert_eq!(run.code, Some(2), "{args:?} {envs:?}: {}", run.stderr);
    }
}

fn fake_ncsa(name: &str, body: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    path
}

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// A key file in the target directory, named after the test that needs it.
fn key_file(name: &str, content: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, content).unwrap();
    path
}

fn spawn_tracer(script: &Path, args: &[&str]) -> std::process::Child {
    let key = key_file("key-valid.txt", &format!("{KEY}\n"));
    command(
        &[
            &["--varnishncsa", script.to_str().unwrap()],
            &["--key-file", key.to_str().unwrap()],
            args,
        ]
        .concat(),
        &[],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap()
}

fn finish(child: std::process::Child) -> Run {
    let started = Instant::now();
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    }
}

fn terminate(child: &std::process::Child) {
    // SAFETY: plain kill(2) on the pid of a child this test owns.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
}

#[test]
fn sigterm_with_a_hanging_endpoint_exits_within_the_grace_period() {
    let addr = hang();
    let url = format!("http://{addr}/v1/traces");
    let body =
        format!("i=0; while [ $i -lt 200 ]; do echo '{LINE}'; i=$((i+1)); done; exec sleep 60");
    let script = fake_ncsa("ncsa-hang.sh", &body);
    let child = spawn_tracer(
        &script,
        &[
            "--endpoint",
            &url,
            "--queue-size",
            "10",
            "--batch-size",
            "1",
        ],
    );
    std::thread::sleep(Duration::from_millis(1000));
    let started = Instant::now();
    terminate(&child);
    let run = finish(child);
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // Every span is accounted for: the in-flight one and the queued ones count as dropped_export
    let (exported, queue, export) = (
        run.counter("exported"),
        run.counter("dropped_queue"),
        run.counter("dropped_export"),
    );
    assert_eq!(exported, 0);
    assert_eq!(queue + export, 200, "{}", run.stderr);
    assert!(export >= 11, "{}", run.stderr);
}

#[test]
fn child_ignoring_sigterm_is_killed() {
    let script = fake_ncsa(
        "ncsa-stubborn.sh",
        "trap '' TERM; while :; do sleep 1; done",
    );
    let fake = serve(|_| http("200 OK", &[], b""));
    let mut child = spawn_tracer(&script, &["--endpoint", &fake.url()]);
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    terminate(&child);
    child.wait().unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed > Duration::from_secs(4), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
}

#[test]
fn varnishncsa_stderr_is_forwarded_and_overruns_counted() {
    let body = format!("echo '{LINE}'; echo 'Log overrun' >&2; echo 'other' >&2");
    let script = fake_ncsa("ncsa-overrun.sh", &body);
    let fake = serve(|_| http("200 OK", &[], b""));
    let out = spawn_tracer(&script, &["--endpoint", &fake.url()])
        .wait_with_output()
        .unwrap();
    let run = Run {
        code: out.status.code(),
        stdout: String::new(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: Duration::ZERO,
    };
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("varnishncsa: Log overrun"));
    assert!(run.stderr.contains("varnishncsa: other"));
    assert_eq!(run.counter("vsl_overruns"), 1);
    assert_eq!(run.counter("exported"), 1);
}

fn span_names(body: &[u8]) -> Vec<(String, i32)> {
    use varnish_tracer_proto::ExportTraceServiceRequest;
    let request = ExportTraceServiceRequest::decode(body).unwrap();
    request
        .resource_spans
        .iter()
        .flat_map(|rs| &rs.scope_spans)
        .flat_map(|ss| &ss.spans)
        .map(|span| (span.name.clone(), span.kind))
        .collect()
}

fn gunzip(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[test]
fn gzip_body_decodes_and_carries_the_header() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let run = run_stdin(
        &["--endpoint", &fake.url(), "--compression", "gzip"],
        &[],
        &lines(3),
    );
    assert_eq!(run.counter("exported"), 3);
    let request = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(request.header("content-encoding"), Some("gzip"));
    assert_eq!(
        request.header("content-type"),
        Some("application/x-protobuf")
    );
    let spans = span_names(&gunzip(&request.body));
    assert_eq!(spans, vec![("GET".to_owned(), 2); 3]);
}

#[test]
fn uncompressed_by_default() {
    let fake = serve(|_| http("200 OK", &[], b""));
    run_stdin(&["--endpoint", &fake.url()], &[], &lines(2));
    let request = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(request.header("content-encoding"), None);
    assert_eq!(span_names(&request.body).len(), 2);
}

#[test]
fn compression_comes_from_the_environment_with_the_traces_variable_first() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let url = fake.url();
    run_stdin(
        &["--endpoint", &url],
        &[("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip")],
        &lines(1),
    );
    run_stdin(
        &["--endpoint", &url],
        &[
            ("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip"),
            ("OTEL_EXPORTER_OTLP_TRACES_COMPRESSION", "none"),
        ],
        &lines(1),
    );
    run_stdin(
        &["--endpoint", &url],
        &[("OTEL_EXPORTER_OTLP_TRACES_COMPRESSION", "gzip")],
        &lines(1),
    );
    let seen = fake.requests.lock().unwrap().clone();
    let encodings: Vec<_> = seen.iter().map(|r| r.header("content-encoding")).collect();
    assert_eq!(encodings, [Some("gzip"), None, Some("gzip")]);
}

#[test]
fn dry_run_prints_spans_and_makes_no_http_request() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let run = run_stdin(&["--endpoint", &fake.url(), "--dry-run"], &[], &lines(2));
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(fake.count(), 0);
    let printed: Vec<serde_json::Value> = run
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(printed.len(), 2);
    assert_eq!(printed[0]["trace_id"], "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(printed[0]["span_id"], "b7ad6b7169203331");
    assert_eq!(printed[0]["name"], "GET");
    assert_eq!(printed[0]["kind"], "SERVER");
    assert_eq!(
        printed[0]["start_time_unix_nano"],
        1_793_455_717_323_367_000u64
    );
    assert_eq!(run.counter("exported"), 2);
}

#[test]
fn dry_run_needs_no_reachable_endpoint() {
    let run = run_stdin(
        &["--endpoint", "http://127.0.0.1:1/v1/traces"],
        &[("VARNISH_TRACER_DRY_RUN", "true")],
        &lines(1),
    );
    assert_eq!(run.code, Some(0));
    assert_eq!(run.stdout.lines().count(), 1);
    assert!(!run.stderr.contains("export failed"), "{}", run.stderr);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One request over a fresh connection; returns the status line and the body.
fn fetch(addr: &str, request: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    let status = reply.lines().next().unwrap_or_default().to_owned();
    let body = reply
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    (status, body)
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn metrics_endpoint_serves_counters_and_404s_everything_else() {
    let fake = serve(|_| http("200 OK", &[], b""));
    let body = format!(
        "i=0; while [ $i -lt 3 ]; do echo '{LINE}'; i=$((i+1)); done; echo garbage; exec sleep 60"
    );
    let script = fake_ncsa("ncsa-metrics.sh", &body);
    let addr = format!("127.0.0.1:{}", free_port());
    let child = spawn_tracer(
        &script,
        &[
            "--endpoint",
            &fake.url(),
            "--batch-size",
            "3",
            "--metrics-listen",
            &addr,
        ],
    );
    wait_for("the listener", || TcpStream::connect(&addr).is_ok());
    wait_for("the export", || fake.count() == 1);
    let get = "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n";
    let mut text = String::new();
    wait_for("the counters", || {
        let (status, body) = fetch(&addr, get);
        assert_eq!(status, "HTTP/1.1 200 OK");
        text = body;
        text.contains("varnish_tracer_spans_exported_total 3\n")
    });
    assert!(
        text.contains("varnish_tracer_lines_malformed_total 1\n"),
        "{text}"
    );
    assert!(text.contains("varnish_tracer_spans_dropped_total{reason=\"queue\"} 0\n"));
    assert!(text.contains("varnish_tracer_spans_dropped_total{reason=\"export\"} 0\n"));
    assert!(text.contains("varnish_tracer_vsl_overruns_total 0\n"));
    assert!(text.contains("varnish_tracer_queue_spans 0\n"));
    assert!(text.contains("varnish_tracer_up 1\n"));
    for request in [
        "GET / HTTP/1.1\r\n\r\n",
        "GET /metrics/x HTTP/1.1\r\n\r\n",
        "GET /metrics?a=1 HTTP/1.1\r\n\r\n",
        "POST /metrics HTTP/1.1\r\n\r\n",
        "garbage\r\n\r\n",
    ] {
        assert_eq!(
            fetch(&addr, request).0,
            "HTTP/1.1 404 Not Found",
            "{request:?}"
        );
    }
    // An oversized request head and a silent client neither crash nor block the endpoint
    let huge = format!(
        "GET /metrics HTTP/1.1\r\nX: {}\r\n\r\n",
        "a".repeat(64 * 1024)
    );
    assert_eq!(fetch(&addr, &huge).0, "HTTP/1.1 404 Not Found");
    let _silent = TcpStream::connect(&addr).unwrap();
    let started = Instant::now();
    assert_eq!(fetch(&addr, get).0, "HTTP/1.1 200 OK");
    assert!(started.elapsed() < Duration::from_secs(5));
    terminate(&child);
    let run = finish(child);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
}

#[test]
fn metrics_listener_problems_exit_with_2() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_addr = taken.local_addr().unwrap().to_string();
    for addr in [taken_addr.as_str(), "not-an-address", "127.0.0.1"] {
        let run = run_stdin(&["--metrics-listen", addr], &[], "");
        assert_eq!(run.code, Some(2), "{addr}: {}", run.stderr);
    }
    let run = run_stdin(&[], &[("VARNISH_TRACER_METRICS_LISTEN", "nonsense")], "");
    assert_eq!(run.code, Some(2));
}

#[test]
fn invalid_key_file_exits_with_2_without_printing_the_key() {
    let script = fake_ncsa("ncsa-never.sh", "exit 0");
    let secret = "SECRET0123456789abcdefghijklmnopqrstuvwxyz";
    let cases = [
        key_file("key-short.txt", "abcd\n"),
        key_file("key-upper.txt", &format!("{}\n", KEY.to_uppercase())),
        key_file("key-secret.txt", &format!("{secret}\n")),
        key_file("key-empty.txt", ""),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("key-missing.txt"),
    ];
    for path in cases {
        let args = [
            "--varnishncsa",
            script.to_str().unwrap(),
            "--key-file",
            path.to_str().unwrap(),
        ];
        let out = command(&args, &[]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{path:?}: {stderr}");
        assert!(stderr.contains("key file"), "{stderr}");
        assert!(
            !stderr.contains(secret) && !stderr.contains(&KEY.to_uppercase()),
            "{stderr}"
        );
    }
    // The environment variable selects the file too
    let bad = key_file("key-env-bad.txt", "nope");
    let out = command(
        &["--varnishncsa", script.to_str().unwrap()],
        &[("VARNISH_TRACER_KEY_FILE", bad.to_str().unwrap())],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // The default path does not exist in the test environment
    let out = command(&["--varnishncsa", script.to_str().unwrap()], &[])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/etc/varnish/tracer.key"));
}

#[test]
fn varnishncsa_gets_the_key_in_query_and_format() {
    let args_file = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ncsa-args.txt");
    let body = format!(
        "for a in \"$@\"; do echo \"$a\"; done > '{}'",
        args_file.display()
    );
    let script = fake_ncsa("ncsa-args.sh", &body);
    let fake = serve(|_| http("200 OK", &[], b""));
    let out = spawn_tracer(&script, &["--endpoint", &fake.url()])
        .wait_with_output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stderr).contains(KEY));
    let args: Vec<String> = std::fs::read_to_string(&args_file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    for flag in ["-c", "-b", "-j"] {
        assert!(args.iter().any(|a| a == flag), "{flag} in {args:?}");
    }
    let after = |flag: &str| &args[args.iter().position(|a| a == flag).unwrap() + 1];
    assert_eq!(
        after("-q"),
        &format!("VCL_Log:{KEY}_tp or VCL_Log:{KEY}_be_tp")
    );
    let format = after("-F");
    assert!(format.contains(&format!("%{{VCL_Log:{KEY}_be_tp}}x")));
    assert!(!format.contains("tracer_"));
}

#[test]
fn ca_file_and_compression_problems_exit_with_2() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let empty = dir.join("ca-empty.pem");
    std::fs::write(&empty, "no certificate in here\n").unwrap();
    let broken = dir.join("ca-broken.pem");
    std::fs::write(&broken, "-----BEGIN CERTIFICATE-----\nAAAA\n").unwrap();
    for (args, envs) in [
        (vec!["--ca-file", "/nonexistent/ca.pem"], vec![]),
        (vec!["--ca-file", empty.to_str().unwrap()], vec![]),
        (vec!["--ca-file", broken.to_str().unwrap()], vec![]),
        (
            vec![],
            vec![("OTEL_EXPORTER_OTLP_CERTIFICATE", empty.to_str().unwrap())],
        ),
        (
            vec![],
            vec![(
                "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE",
                "/nonexistent/ca.pem",
            )],
        ),
        (vec!["--compression", "zstd"], vec![]),
        (vec![], vec![("OTEL_EXPORTER_OTLP_COMPRESSION", "zstd")]),
    ] {
        let run = run_stdin(&args, &envs, "");
        assert_eq!(run.code, Some(2), "{args:?} {envs:?}: {}", run.stderr);
    }
}
