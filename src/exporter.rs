//! Batching exporter thread: collects spans, posts them over OTLP/HTTP and keeps the counters.

use crate::config::Compression;
use crate::otlp::{
    ExportTraceServiceRequest, ExportTraceServiceResponse, InstrumentationScope, KeyValue,
    Resource, ResourceSpans, ScopeSpans, Span,
};
use crate::text::escape_log;
use prost::Message;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use ureq::tls::{Certificate, RootCerts, TlsConfig};

const REPORT_INTERVAL: Duration = Duration::from_secs(60);
const BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const RETRYABLE: [u16; 4] = [429, 502, 503, 504];
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const SCOPE_NAME: &str = "varnish-tracer";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Longest time the exporter keeps flushing once shutdown is requested.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Set once, to the instant shutdown was requested.
pub type Shutdown = OnceLock<Instant>;

#[derive(Default)]
pub struct Counters {
    pub exported: AtomicU64,
    pub dropped_queue: AtomicU64,
    pub dropped_export: AtomicU64,
    pub malformed: AtomicU64,
    pub vsl_overruns: AtomicU64,
    /// Spans in the queue: incremented before a send and decremented when the exporter receives one.
    pub queued: AtomicU64,
    /// Spans the exporter owns, in the batch being built or sent, until their outcome is counted.
    pub held: AtomicU64,
}

impl Counters {
    /// Counts the spans nobody will export any more as `dropped_export`; returns how many there were.
    /// A batch the exporter settles afterwards is not counted a second time.
    pub fn abandon(&self) -> u64 {
        let lost = self.held.swap(0, Relaxed) + self.queued.swap(0, Relaxed);
        self.dropped_export.fetch_add(lost, Relaxed);
        lost
    }
}

pub struct Config {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub batch_size: usize,
    pub batch_timeout: Duration,
    pub export_timeout: Duration,
    pub service_name: String,
    pub resource_attributes: Vec<(String, String)>,
    pub compression: Compression,
    /// Trust roots replacing the bundled ones for https endpoints.
    pub ca_certificates: Option<Vec<Certificate<'static>>>,
    /// Print spans on stdout instead of exporting them.
    pub dry_run: bool,
}

/// Builds the resource; `service.name` and `telemetry.sdk.*` in `extra` are ignored.
pub fn resource(service_name: &str, extra: &[(String, String)]) -> Resource {
    let mut attributes = vec![
        KeyValue::string("service.name", service_name),
        KeyValue::string("telemetry.sdk.name", "varnish-tracer"),
        KeyValue::string("telemetry.sdk.language", "rust"),
        KeyValue::string("telemetry.sdk.version", VERSION),
    ];
    attributes.extend(
        extra
            .iter()
            .filter(|(k, _)| k != "service.name" && !k.starts_with("telemetry.sdk."))
            .map(|(k, v)| KeyValue::string(k, v)),
    );
    Resource {
        attributes,
        dropped_attributes_count: 0,
    }
}

pub fn build_request(resource: &Resource, spans: Vec<Span>) -> ExportTraceServiceRequest {
    let scope = InstrumentationScope {
        name: SCOPE_NAME.to_owned(),
        version: VERSION.to_owned(),
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource.clone()),
            scope_spans: vec![ScopeSpans {
                scope: Some(scope),
                spans,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

/// Gzip stream of `body`, `None` when compression fails.
pub fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

fn build_agent(cfg: &Config) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(cfg.export_timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None);
    if let Some(certificates) = &cfg.ca_certificates {
        let roots = RootCerts::Specific(Arc::new(certificates.clone()));
        builder = builder.tls_config(TlsConfig::builder().root_certs(roots).build());
    }
    builder.build().into()
}

/// Runs until the channel disconnects, then flushes what is left; once `shutdown` is set the flush
/// is limited to `SHUTDOWN_GRACE` and does not retry. In dry-run mode it prints the spans instead.
pub fn run(rx: Receiver<Span>, cfg: Config, counters: &Counters, shutdown: &Shutdown) {
    if cfg.dry_run {
        return dry_run(rx, counters);
    }
    let resource = resource(&cfg.service_name, &cfg.resource_attributes);
    let sink = Sink {
        agent: build_agent(&cfg),
        cfg: &cfg,
        counters,
        shutdown,
    };

    let mut batch: Vec<Span> = Vec::new();
    let mut batch_deadline = Instant::now();
    let mut next_report = Instant::now() + REPORT_INTERVAL;
    loop {
        let now = Instant::now();
        let wake = if batch.is_empty() {
            next_report
        } else {
            batch_deadline.min(next_report)
        };
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(span) => {
                counters.held.fetch_add(1, Relaxed);
                counters.queued.fetch_sub(1, Relaxed);
                if batch.is_empty() {
                    batch_deadline = Instant::now() + cfg.batch_timeout;
                }
                batch.push(span);
                if batch.len() >= cfg.batch_size {
                    sink.send(&resource, std::mem::take(&mut batch));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() && Instant::now() >= batch_deadline {
                    sink.send(&resource, std::mem::take(&mut batch));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    sink.send(&resource, std::mem::take(&mut batch));
                }
                report(counters);
                return;
            }
        }
        if Instant::now() >= next_report {
            report(counters);
            next_report = Instant::now() + REPORT_INTERVAL;
        }
    }
}

/// Prints every span as it arrives; nothing leaves the process over the network.
fn dry_run(rx: Receiver<Span>, counters: &Counters) {
    let mut next_report = Instant::now() + REPORT_INTERVAL;
    loop {
        match rx.recv_timeout(next_report.saturating_duration_since(Instant::now())) {
            Ok(span) => {
                counters.queued.fetch_sub(1, Relaxed);
                let line = crate::dryrun::json_line(&span);
                // A closed stdout must not stop the pipeline
                let _ = writeln!(std::io::stdout().lock(), "{line}");
                counters.exported.fetch_add(1, Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                report(counters);
                return;
            }
        }
        if Instant::now() >= next_report {
            report(counters);
            next_report = Instant::now() + REPORT_INTERVAL;
        }
    }
}

pub fn report(c: &Counters) {
    eprintln!(
        "varnish-tracer: exported={} dropped_queue={} dropped_export={} malformed={} vsl_overruns={}",
        c.exported.load(Relaxed),
        c.dropped_queue.load(Relaxed),
        c.dropped_export.load(Relaxed),
        c.malformed.load(Relaxed),
        c.vsl_overruns.load(Relaxed)
    );
}

struct Sink<'a> {
    agent: ureq::Agent,
    cfg: &'a Config,
    counters: &'a Counters,
    shutdown: &'a Shutdown,
}

enum Outcome {
    /// Accepted by the receiver, with this many spans rejected.
    Done(u64),
    Retry(Option<Duration>),
    Fatal,
}

/// Rejected span count and message from a 2xx response body; an empty or undecodable body means full success.
fn partial_success(body: &[u8], count: u64) -> (u64, String) {
    let Ok(response) = ExportTraceServiceResponse::decode(body) else {
        return (0, String::new());
    };
    response.partial_success.map_or((0, String::new()), |p| {
        let rejected = u64::try_from(p.rejected_spans).unwrap_or(0).min(count);
        (rejected, p.error_message)
    })
}

/// `Retry-After` in seconds form on 429 and 503, capped at `MAX_RETRY_AFTER`.
fn retry_after(status: u16, value: Option<&str>) -> Option<Duration> {
    if !matches!(status, 429 | 503) {
        return None;
    }
    let seconds = value?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// Errors that no retry can fix: bad request, URL or header, and redirects.
fn is_fatal(err: &ureq::Error) -> bool {
    use ureq::Error::{
        BadUri, Http, InvalidProxyUrl, RedirectFailed, RequireHttpsOnly, TooManyRedirects,
    };
    matches!(
        err,
        Http(_)
            | BadUri(_)
            | InvalidProxyUrl
            | RedirectFailed
            | TooManyRedirects
            | RequireHttpsOnly(_)
    )
}

impl Sink<'_> {
    /// Time left in the shutdown grace period, `None` while no shutdown is requested.
    fn grace_left(&self) -> Option<Duration> {
        let requested = self.shutdown.get()?;
        Some((*requested + SHUTDOWN_GRACE).saturating_duration_since(Instant::now()))
    }

    /// Sleeps for `delay`, or until shutdown is requested.
    fn pause(&self, delay: Duration) {
        let end = Instant::now() + delay;
        while self.shutdown.get().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send(&self, resource: &Resource, spans: Vec<Span>) {
        let count = spans.len() as u64;
        let plain = build_request(resource, spans).encode_to_vec();
        let (body, gzipped) = match self.cfg.compression {
            Compression::Gzip => gzip(&plain).map_or((plain, false), |body| (body, true)),
            Compression::None => (plain, false),
        };
        for attempt in 0..=BACKOFFS.len() {
            let timeout = match self.grace_left() {
                Some(left) if left.is_zero() => break,
                Some(left) => left.min(self.cfg.export_timeout),
                None => self.cfg.export_timeout,
            };
            let delay = match self.post(&body, gzipped, count, timeout) {
                Outcome::Done(rejected) => {
                    self.settle(count, count - rejected, rejected);
                    return;
                }
                Outcome::Fatal => break,
                Outcome::Retry(after) => after.or(BACKOFFS.get(attempt).copied()),
            };
            let Some(delay) = delay.filter(|_| self.shutdown.get().is_none()) else {
                break;
            };
            self.pause(delay);
            if self.shutdown.get().is_some() {
                break;
            }
        }
        if self.settle(count, 0, count) {
            eprintln!("varnish-tracer: dropped batch of {count} spans after failed export");
        }
    }

    /// Counts the outcome of a batch unless `Counters::abandon` already counted it; true when counted.
    fn settle(&self, count: u64, exported: u64, dropped: u64) -> bool {
        let owned = self
            .counters
            .held
            .try_update(Relaxed, Relaxed, |held| held.checked_sub(count))
            .is_ok();
        if owned {
            self.counters.exported.fetch_add(exported, Relaxed);
            self.counters.dropped_export.fetch_add(dropped, Relaxed);
        }
        owned
    }

    fn post(&self, body: &[u8], gzipped: bool, count: u64, timeout: Duration) -> Outcome {
        let mut request = self
            .agent
            .post(&self.cfg.endpoint)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Content-Type", "application/x-protobuf");
        if gzipped {
            request = request.header("Content-Encoding", "gzip");
        }
        for (key, value) in &self.cfg.headers {
            request = request.header(key, value);
        }
        match request.send(body) {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let wait = retry_after(
                    status,
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok()),
                );
                let mut text = Vec::new();
                let _ = response
                    .body_mut()
                    .as_reader()
                    .take(MAX_RESPONSE_BYTES)
                    .read_to_end(&mut text);
                if (200..300).contains(&status) {
                    return Outcome::Done(self.rejected(count, &text));
                }
                eprintln!(
                    "varnish-tracer: export failed: HTTP {status}: {}",
                    escape_log(&String::from_utf8_lossy(&text))
                );
                if RETRYABLE.contains(&status) {
                    Outcome::Retry(wait)
                } else {
                    Outcome::Fatal
                }
            }
            Err(err) => {
                eprintln!(
                    "varnish-tracer: export failed: {}",
                    escape_log(&err.to_string())
                );
                if is_fatal(&err) {
                    Outcome::Fatal
                } else {
                    Outcome::Retry(None)
                }
            }
        }
    }

    /// Spans the receiver reports as rejected in an accepted request, logged with its message.
    fn rejected(&self, count: u64, body: &[u8]) -> u64 {
        let (rejected, message) = partial_success(body, count);
        if rejected > 0 || !message.is_empty() {
            eprintln!(
                "varnish-tracer: receiver rejected {rejected} of {count} spans: {}",
                escape_log(&message)
            );
        }
        rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::ExportTracePartialSuccess;

    #[test]
    fn request_carries_all_spans_under_one_resource() {
        let res = resource("svc", &[("a".into(), "b".into())]);
        let req = build_request(&res, vec![Span::default(), Span::default()]);
        assert_eq!(req.resource_spans.len(), 1);
        let rs = &req.resource_spans[0];
        assert_eq!(rs.scope_spans.len(), 1);
        assert_eq!(rs.scope_spans[0].spans.len(), 2);
        assert_eq!(
            rs.scope_spans[0].scope.as_ref().unwrap().name,
            "varnish-tracer"
        );
        let keys: Vec<_> = res.attributes.iter().map(|kv| kv.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "service.name",
                "telemetry.sdk.name",
                "telemetry.sdk.language",
                "telemetry.sdk.version",
                "a"
            ]
        );
    }

    #[test]
    fn resource_ignores_reserved_keys() {
        let extra = [
            ("service.name", "other"),
            ("telemetry.sdk.name", "x"),
            ("telemetry.sdk.custom", "x"),
            ("team", "a"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let res = resource("svc", &extra);
        let keys: Vec<_> = res.attributes.iter().map(|kv| kv.key.as_str()).collect();
        assert_eq!(keys.iter().filter(|k| **k == "service.name").count(), 1);
        assert_eq!(
            keys.iter()
                .filter(|k| k.starts_with("telemetry.sdk."))
                .count(),
            3
        );
        assert!(keys.contains(&"team"));
    }

    fn response(rejected: i64, message: &str) -> Vec<u8> {
        ExportTraceServiceResponse {
            partial_success: Some(ExportTracePartialSuccess {
                rejected_spans: rejected,
                error_message: message.to_owned(),
            }),
        }
        .encode_to_vec()
    }

    #[test]
    fn partial_success_counts_rejected_spans() {
        assert_eq!(partial_success(&[], 10), (0, String::new()));
        assert_eq!(partial_success(&[0xff, 0xff], 10), (0, String::new()));
        assert_eq!(
            partial_success(&response(3, "bad"), 10),
            (3, "bad".to_owned())
        );
        assert_eq!(partial_success(&response(99, ""), 10).0, 10);
        assert_eq!(partial_success(&response(-5, ""), 10).0, 0);
        let empty = ExportTraceServiceResponse::default().encode_to_vec();
        assert_eq!(partial_success(&empty, 10), (0, String::new()));
    }

    #[test]
    fn retry_after_seconds() {
        let secs = Duration::from_secs;
        assert_eq!(retry_after(429, Some("7")), Some(secs(7)));
        assert_eq!(retry_after(503, Some(" 2 ")), Some(secs(2)));
        assert_eq!(retry_after(429, Some("3600")), Some(secs(30)));
        assert_eq!(
            retry_after(429, Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(retry_after(429, None), None);
        assert_eq!(retry_after(502, Some("5")), None);
    }

    #[test]
    fn error_classification() {
        assert!(is_fatal(&ureq::Error::BadUri("x".into())));
        assert!(is_fatal(&ureq::Error::TooManyRedirects));
        assert!(is_fatal(&ureq::Error::RedirectFailed));
        assert!(!is_fatal(&ureq::Error::HostNotFound));
        assert!(!is_fatal(&ureq::Error::ConnectionFailed));
        assert!(!is_fatal(&ureq::Error::Timeout(ureq::Timeout::Global)));
    }

    #[test]
    fn gzip_round_trip() {
        let res = resource("svc", &[]);
        let plain = build_request(&res, vec![Span::default(); 50]).encode_to_vec();
        let compressed = gzip(&plain).unwrap();
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        assert!(compressed.len() < plain.len());
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, plain);
        let request = ExportTraceServiceRequest::decode(decoded.as_slice()).unwrap();
        assert_eq!(request.resource_spans[0].scope_spans[0].spans.len(), 50);
        assert!(gzip(&[]).is_some());
    }

    fn sink<'a>(cfg: &'a Config, counters: &'a Counters, shutdown: &'a Shutdown) -> Sink<'a> {
        Sink {
            agent: build_agent(cfg),
            cfg,
            counters,
            shutdown,
        }
    }

    fn config() -> Config {
        Config {
            endpoint: "http://127.0.0.1:1/".into(),
            headers: vec![],
            batch_size: 1,
            batch_timeout: Duration::from_secs(1),
            export_timeout: Duration::from_secs(1),
            service_name: "svc".into(),
            resource_attributes: vec![],
            compression: Compression::None,
            ca_certificates: None,
            dry_run: false,
        }
    }

    #[test]
    fn abandon_counts_held_and_queued_spans_once() {
        let (cfg, counters, shutdown) = (config(), Counters::default(), Shutdown::new());
        counters.held.store(5, Relaxed);
        counters.queued.store(7, Relaxed);
        counters.dropped_export.store(1, Relaxed);
        assert_eq!(counters.abandon(), 12);
        assert_eq!(counters.dropped_export.load(Relaxed), 13);
        assert_eq!(counters.queued.load(Relaxed), 0);
        // The exporter settling its batch afterwards must not count it again
        assert!(!sink(&cfg, &counters, &shutdown).settle(5, 5, 0));
        assert_eq!(counters.exported.load(Relaxed), 0);
        assert_eq!(counters.dropped_export.load(Relaxed), 13);
        assert_eq!(counters.abandon(), 0);
    }

    #[test]
    fn settle_counts_the_outcome_of_a_held_batch() {
        let (cfg, counters, shutdown) = (config(), Counters::default(), Shutdown::new());
        counters.held.store(10, Relaxed);
        let sink = sink(&cfg, &counters, &shutdown);
        assert!(sink.settle(4, 3, 1));
        assert!(sink.settle(6, 0, 6));
        assert_eq!(counters.held.load(Relaxed), 0);
        assert_eq!(counters.exported.load(Relaxed), 3);
        assert_eq!(counters.dropped_export.load(Relaxed), 7);
    }

    #[test]
    fn agent_builds_with_custom_roots() {
        let path = crate::text::test_dir().join("ca-exporter.pem");
        std::fs::write(&path, crate::config::TEST_CA_PEM).unwrap();
        let mut cfg = config();
        cfg.ca_certificates = Some(crate::config::load_ca(&path).unwrap());
        build_agent(&cfg);
    }
}
