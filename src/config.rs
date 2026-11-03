//! Command line, environment and OpenTelemetry variable parsing.

use clap::{Parser, ValueEnum};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use ureq::http::{HeaderName, Uri};
use ureq::tls::{Certificate, PemItem, parse_pem};

const DEFAULT_ENDPOINT: &str = "http://localhost:14499/otlp/v1/traces";
const MAX_CA_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Request body compression.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Compression {
    #[default]
    None,
    Gzip,
}

#[derive(Parser, Debug)]
#[command(version, about = "Turns varnishncsa JSON lines into OTLP spans")]
pub struct Cli {
    /// OTLP/HTTP traces URL; falls back to OTEL_EXPORTER_OTLP_ENDPOINT + /v1/traces, then to the OneAgent local ingest
    #[arg(long, env = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")]
    pub endpoint: Option<String>,

    /// Extra request header as NAME=VALUE (repeatable, never split on commas); OTEL_EXPORTER_OTLP_HEADERS is added first
    #[arg(long = "header")]
    pub headers: Vec<String>,

    /// service.name resource attribute
    #[arg(long, env = "OTEL_SERVICE_NAME", default_value = "varnish")]
    pub service_name: String,

    /// Varnish working directory, passed to varnishncsa -n
    #[arg(
        short = 'n',
        long,
        env = "VARNISH_TRACER_INSTANCE",
        default_value = "/var/lib/varnish/varnishd"
    )]
    pub instance: String,

    /// Path of the varnishncsa binary
    #[arg(
        long,
        env = "VARNISH_TRACER_VARNISHNCSA",
        default_value = "varnishncsa"
    )]
    pub varnishncsa: String,

    /// Read varnishncsa JSON lines from stdin instead of starting varnishncsa
    #[arg(long)]
    pub stdin: bool,

    /// Add the url.query attribute; query strings may carry personal data
    #[arg(long, env = "VARNISH_TRACER_INCLUDE_QUERY")]
    pub include_query: bool,

    /// Spans per export request
    #[arg(long, env = "VARNISH_TRACER_BATCH_SIZE", default_value_t = 512,
        value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub batch_size: u32,

    /// Longest wait before exporting a partial batch
    #[arg(long, env = "VARNISH_TRACER_BATCH_TIMEOUT_MS", default_value_t = 5000,
        value_parser = clap::value_parser!(u64).range(1..=60_000))]
    pub batch_timeout_ms: u64,

    /// Spans buffered before new ones are dropped
    #[arg(long, env = "VARNISH_TRACER_QUEUE_SIZE", default_value_t = 8192,
        value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    pub queue_size: u32,

    /// Timeout of one export request
    #[arg(
        long,
        env = "VARNISH_TRACER_EXPORT_TIMEOUT_MS",
        default_value_t = 10000,
        value_parser = clap::value_parser!(u64).range(100..=120_000)
    )]
    pub export_timeout_ms: u64,

    /// File with the shared key that prefixes the VCL log records; not read with --stdin
    #[arg(
        long,
        env = "VARNISH_TRACER_KEY_FILE",
        default_value = "/etc/varnish/tracer.key"
    )]
    pub key_file: PathBuf,

    /// Print each span as one JSON line on stdout instead of exporting it
    #[arg(long, env = "VARNISH_TRACER_DRY_RUN")]
    pub dry_run: bool,

    /// Serve Prometheus metrics on ADDR:PORT, off by default
    #[arg(long, env = "VARNISH_TRACER_METRICS_LISTEN")]
    pub metrics_listen: Option<SocketAddr>,

    /// Request compression, none or gzip; falls back to OTEL_EXPORTER_OTLP_COMPRESSION, then none
    #[arg(long, value_enum, env = "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION")]
    pub compression: Option<Compression>,

    /// PEM file with the CA certificates that replace the bundled roots for https endpoints; falls back to OTEL_EXPORTER_OTLP_CERTIFICATE
    #[arg(long, env = "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE")]
    pub ca_file: Option<PathBuf>,
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| Some(hex(bytes[i + 1])? * 16 + hex(bytes[i + 2])?))
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte as u8);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `k=v` pairs, trimming keys and percent-decoding values; entries without `=` or with an empty key are skipped.
pub fn parse_pairs<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    items
        .into_iter()
        .filter_map(|item| {
            let (key, value) = item.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_owned(), percent_decode(value.trim())))
        })
        .collect()
}

pub fn parse_resource_attributes(text: &str) -> Vec<(String, String)> {
    parse_pairs(text.split(','))
}

/// Parses `NAME=VALUE` headers; the error never repeats the entry, which may hold a credential.
pub fn parse_headers<'a>(
    items: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<(String, String)>, String> {
    items
        .into_iter()
        .map(|item| {
            let (name, value) = item
                .split_once('=')
                .ok_or("header without '=', expected NAME=VALUE")?;
            let name = name.trim();
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name {name:?}"))?;
            let value = percent_decode(value.trim());
            if value.chars().any(char::is_control) {
                return Err(format!(
                    "header {name} has a control character in its value"
                ));
            }
            Ok((name.to_owned(), value))
        })
        .collect()
}

pub fn resolve_endpoint(explicit: Option<&str>, base: Option<&str>) -> String {
    let non_empty = |v: &&str| !v.is_empty();
    match (explicit.filter(non_empty), base.filter(non_empty)) {
        (Some(url), _) => url.to_owned(),
        (None, Some(base)) => format!("{}/v1/traces", base.trim_end_matches('/')),
        (None, None) => DEFAULT_ENDPOINT.to_owned(),
    }
}

/// The endpoint must be an absolute http or https URL with a host.
pub fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    let uri: Uri = endpoint
        .parse()
        .map_err(|err| format!("invalid endpoint: {err}"))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err("endpoint must be an absolute http:// or https:// URL".to_owned());
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err("endpoint has no host".to_owned());
    }
    Ok(())
}

/// The explicit setting wins; otherwise `OTEL_EXPORTER_OTLP_COMPRESSION` is used, with `none` as the default.
pub fn resolve_compression(
    explicit: Option<Compression>,
    fallback: Option<&str>,
) -> Result<Compression, String> {
    if let Some(compression) = explicit {
        return Ok(compression);
    }
    match fallback.map(str::trim).filter(|v| !v.is_empty()) {
        None | Some("none") => Ok(Compression::None),
        Some("gzip") => Ok(Compression::Gzip),
        Some(other) => Err(format!(
            "invalid OTEL_EXPORTER_OTLP_COMPRESSION {other:?}, expected none or gzip"
        )),
    }
}

/// The explicit path wins over the fallback; an empty value counts as unset.
pub fn resolve_ca_file(explicit: Option<&Path>, fallback: Option<&str>) -> Option<PathBuf> {
    explicit
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| fallback.filter(|v| !v.is_empty()).map(PathBuf::from))
}

/// Reads the PEM trust roots; the file must hold at least one certificate and nothing malformed.
pub fn load_ca(path: &Path) -> Result<Vec<Certificate<'static>>, String> {
    use std::io::Read;
    let shown = path.display();
    let mut pem = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_CA_FILE_BYTES + 1).read_to_end(&mut pem))
        .map_err(|err| format!("CA file {shown}: {err}"))?;
    if pem.len() as u64 > MAX_CA_FILE_BYTES {
        return Err(format!(
            "CA file {shown}: larger than {MAX_CA_FILE_BYTES} bytes"
        ));
    }
    let mut certificates = Vec::new();
    for item in parse_pem(&pem) {
        let item = item.map_err(|err| format!("CA file {shown}: {err}"))?;
        if let PemItem::Certificate(certificate) = item {
            certificates.push(certificate);
        }
    }
    if certificates.is_empty() {
        return Err(format!("CA file {shown}: no PEM certificate found"));
    }
    Ok(certificates)
}

impl Cli {
    pub fn endpoint(&self) -> Result<String, String> {
        let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        let endpoint = resolve_endpoint(self.endpoint.as_deref(), base.as_deref());
        validate_endpoint(&endpoint)?;
        Ok(endpoint)
    }

    /// Headers from `OTEL_EXPORTER_OTLP_HEADERS` (comma separated) followed by the `--header` flags.
    pub fn header_pairs(&self) -> Result<Vec<(String, String)>, String> {
        let env = std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default();
        let from_env = env.split(',').filter(|item| !item.trim().is_empty());
        parse_headers(from_env.chain(self.headers.iter().map(String::as_str)))
    }

    pub fn compression(&self) -> Result<Compression, String> {
        let fallback = std::env::var("OTEL_EXPORTER_OTLP_COMPRESSION").ok();
        resolve_compression(self.compression, fallback.as_deref())
    }

    /// The trust roots from the CA file, `None` when no file is configured.
    pub fn ca_certificates(&self) -> Result<Option<Vec<Certificate<'static>>>, String> {
        let fallback = std::env::var("OTEL_EXPORTER_OTLP_CERTIFICATE").ok();
        resolve_ca_file(self.ca_file.as_deref(), fallback.as_deref())
            .map(|path| load_ca(&path))
            .transpose()
    }

    pub fn resource_attributes(&self) -> Vec<(String, String)> {
        std::env::var("OTEL_RESOURCE_ATTRIBUTES")
            .map(|v| parse_resource_attributes(&v))
            .unwrap_or_default()
    }
}

/// A throwaway self-signed certificate for tests.
#[cfg(test)]
pub const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBkzCCATmgAwIBAgIUGXkV3fOgvpGphphKP+hGUHs/t7owCgYIKoZIzj0EAwIw\nHjEcMBoGA1UEAwwTdmFybmlzaC10cmFjZXItdGVzdDAgFw0yNjExMDMxNTEwMDBa\nGA8yMTI2MTAxMDE1MTAwMFowHjEcMBoGA1UEAwwTdmFybmlzaC10cmFjZXItdGVz\ndDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABEwl1QhzS9dmZz9PZH3jrykAw7s7\n0rH6lfb9bmKt5LEIh+Mj05rVDWc32Lgl2F00COZ9/Esl0t3MpAdcfgkc8E6jUzBR\nMB0GA1UdDgQWBBRsn2UI7jiyn3fE75M50YqraVV6BDAfBgNVHSMEGDAWgBRsn2UI\n7jiyn3fE75M50YqraVV6BDAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gA\nMEUCIGmjI21Vg8Q9I4n50/dNqCQefSDeE1/PKiHaVK2a7p3NAiEA2/L+A4XBjyVP\nEDD/qeqSU5yYY4ltvEy6ze54Za3g2wY=\n-----END CERTIFICATE-----\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_pairs_are_skipped() {
        let pairs = parse_pairs(["novalue", "=x", " k = v ", ""]);
        assert_eq!(pairs, vec![("k".to_owned(), "v".to_owned())]);
    }

    #[test]
    fn value_keeps_inner_equals_and_bad_escapes() {
        let pairs = parse_pairs(["k=a=b", "p=100%", "q=%zz", "r=%4"]);
        assert_eq!(pairs[0].1, "a=b");
        assert_eq!(pairs[1].1, "100%");
        assert_eq!(pairs[2].1, "%zz");
        assert_eq!(pairs[3].1, "%4");
    }

    #[test]
    fn resource_attributes() {
        let attrs = parse_resource_attributes("deployment.environment=prod,team=a%2Cb");
        assert_eq!(attrs[0], ("deployment.environment".into(), "prod".into()));
        assert_eq!(attrs[1], ("team".into(), "a,b".into()));
    }

    #[test]
    fn headers_are_decoded() {
        let pairs = parse_headers(["Authorization=Api-Token%20abc%3D", "X-Other = v"]).unwrap();
        assert_eq!(pairs[0], ("Authorization".into(), "Api-Token abc=".into()));
        assert_eq!(pairs[1], ("X-Other".into(), "v".into()));
    }

    #[test]
    fn headers_are_validated() {
        assert!(parse_headers(["novalue"]).is_err());
        assert!(parse_headers([""]).is_err());
        assert!(parse_headers(["=x"]).is_err());
        assert!(parse_headers(["bad name=x"]).is_err());
        assert!(parse_headers(["k=a%0d%0ab"]).is_err());
        assert!(parse_headers(["k=a\u{7f}"]).is_err());
        let err = parse_headers(["secret-token"]).unwrap_err();
        assert!(!err.contains("secret-token"));
    }

    #[test]
    fn header_flag_is_not_split_on_commas() {
        let cli = Cli::parse_from(["t", "--header", "A=1,B=2", "--header", "C=3"]);
        assert_eq!(cli.headers, ["A=1,B=2", "C=3"]);
    }

    #[test]
    fn endpoint_resolution() {
        assert_eq!(resolve_endpoint(None, None), DEFAULT_ENDPOINT);
        assert_eq!(
            resolve_endpoint(Some("http://a/x"), Some("http://b")),
            "http://a/x"
        );
        assert_eq!(
            resolve_endpoint(None, Some("http://b:4318/")),
            "http://b:4318/v1/traces"
        );
        assert_eq!(
            resolve_endpoint(Some(""), Some("http://b")),
            "http://b/v1/traces"
        );
        assert_eq!(resolve_endpoint(Some(""), Some("")), DEFAULT_ENDPOINT);
    }

    #[test]
    fn endpoint_validation() {
        assert!(validate_endpoint(DEFAULT_ENDPOINT).is_ok());
        assert!(validate_endpoint("https://example.com:4318/v1/traces").is_ok());
        for bad in [
            "",
            "localhost:4318",
            "/v1/traces",
            "ftp://h/x",
            "http://",
            "http:///x",
            "not a url",
        ] {
            assert!(validate_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn defaults_and_ranges() {
        let cli = Cli::parse_from(["t"]);
        assert_eq!(cli.endpoint, None);
        assert_eq!(cli.queue_size, 8192);
        for args in [
            ["--queue-size", "0"],
            ["--queue-size", "1000001"],
            ["--batch-size", "0"],
            ["--batch-size", "10001"],
            ["--batch-timeout-ms", "0"],
            ["--batch-timeout-ms", "60001"],
            ["--export-timeout-ms", "99"],
            ["--export-timeout-ms", "120001"],
        ] {
            assert!(
                Cli::try_parse_from(["t", args[0], args[1]]).is_err(),
                "{args:?}"
            );
        }
        assert!(
            Cli::try_parse_from(["t", "--export-timeout-ms", "100", "--queue-size", "1000000"])
                .is_ok()
        );
    }

    #[test]
    fn compression_resolution() {
        use Compression::{Gzip, None};
        assert_eq!(resolve_compression(Option::None, Option::None), Ok(None));
        assert_eq!(resolve_compression(Option::None, Some("gzip")), Ok(Gzip));
        assert_eq!(resolve_compression(Option::None, Some(" none ")), Ok(None));
        assert_eq!(resolve_compression(Option::None, Some("")), Ok(None));
        assert_eq!(resolve_compression(Some(None), Some("gzip")), Ok(None));
        assert_eq!(resolve_compression(Some(Gzip), Some("none")), Ok(Gzip));
        let err = resolve_compression(Option::None, Some("zstd")).unwrap_err();
        assert!(err.contains("zstd"));
    }

    #[test]
    fn compression_flag_values() {
        let cli = Cli::parse_from(["t", "--compression", "gzip"]);
        assert_eq!(cli.compression, Some(Compression::Gzip));
        assert_eq!(Cli::parse_from(["t"]).compression, Option::None);
        assert!(Cli::try_parse_from(["t", "--compression", "zstd"]).is_err());
    }

    #[test]
    fn ca_file_resolution() {
        let flag = Path::new("/a.pem");
        assert_eq!(
            resolve_ca_file(Some(flag), Some("/b.pem")),
            Some("/a.pem".into())
        );
        assert_eq!(resolve_ca_file(None, Some("/b.pem")), Some("/b.pem".into()));
        assert_eq!(
            resolve_ca_file(Some(Path::new("")), Some("/b.pem")),
            Some("/b.pem".into())
        );
        assert_eq!(resolve_ca_file(None, Some("")), None);
        assert_eq!(resolve_ca_file(None, None), None);
    }

    #[test]
    fn ca_file_validation() {
        let dir = crate::text::test_dir();
        let write = |name: &str, content: &str| {
            let path = dir.join(name);
            std::fs::write(&path, content).unwrap();
            path
        };
        let good = write("ca-good.pem", TEST_CA_PEM);
        assert_eq!(load_ca(&good).unwrap().len(), 1);
        let two = write("ca-two.pem", &format!("{TEST_CA_PEM}\n{TEST_CA_PEM}"));
        assert_eq!(load_ca(&two).unwrap().len(), 2);
        let with_text = write("ca-text.pem", &format!("subject=CN=x\n{TEST_CA_PEM}"));
        assert_eq!(load_ca(&with_text).unwrap().len(), 1);
        for (name, content) in [
            ("ca-none.pem", "just text\n"),
            ("ca-empty.pem", ""),
            ("ca-truncated.pem", "-----BEGIN CERTIFICATE-----\nAAAA\n"),
        ] {
            let err = load_ca(&write(name, content)).unwrap_err();
            assert!(err.contains(name), "{err}");
        }
        assert!(load_ca(&dir.join("ca-missing.pem")).is_err());
        let big = write("ca-big.pem", &"x".repeat(MAX_CA_FILE_BYTES as usize + 1));
        assert!(load_ca(&big).unwrap_err().contains("larger"));
    }

    #[test]
    fn new_options_have_defaults_and_validate() {
        let cli = Cli::parse_from(["t"]);
        assert_eq!(cli.key_file, Path::new("/etc/varnish/tracer.key"));
        assert!(!cli.dry_run);
        assert_eq!(cli.metrics_listen, Option::None);
        assert_eq!(cli.ca_file, Option::None);
        let cli = Cli::parse_from([
            "t",
            "--key-file",
            "/k",
            "--dry-run",
            "--metrics-listen",
            "127.0.0.1:9132",
            "--ca-file",
            "/ca.pem",
        ]);
        assert_eq!(cli.key_file, Path::new("/k"));
        assert!(cli.dry_run);
        assert_eq!(cli.metrics_listen, Some("127.0.0.1:9132".parse().unwrap()));
        assert_eq!(cli.ca_file, Some("/ca.pem".into()));
        for bad in ["9132", "localhost:9132", "127.0.0.1:70000", ""] {
            assert!(
                Cli::try_parse_from(["t", "--metrics-listen", bad]).is_err(),
                "{bad}"
            );
        }
    }
}
