//! Mapping of a [`Record`] to an OTLP span: SERVER for a client transaction, CLIENT for a backend fetch.

use crate::otlp::{KeyValue, SPAN_KIND_CLIENT, SPAN_KIND_SERVER, STATUS_CODE_ERROR, Span, Status};
use crate::record::{Record, Side};
use crate::text::{MAX_VALUE_BYTES, truncate};

pub fn to_span(record: &Record, include_query: bool) -> Span {
    let (kind, attrs) = match record.side {
        Side::Client => (SPAN_KIND_SERVER, server_attributes(record, include_query)),
        Side::Backend => (SPAN_KIND_CLIENT, backend_attributes(record)),
    };
    let error = record.status.is_some_and(|s| s >= 500) || record.fetch_error;
    Span {
        trace_id: record.trace_id.to_vec(),
        span_id: record.span_id.to_vec(),
        parent_span_id: record.parent_id.map(|p| p.to_vec()).unwrap_or_default(),
        name: record
            .method
            .as_deref()
            .map_or("HTTP", |m| truncate(m, MAX_VALUE_BYTES))
            .to_owned(),
        kind,
        start_time_unix_nano: record.start_ns,
        end_time_unix_nano: record.end_ns,
        attributes: attrs,
        status: error.then(|| Status {
            message: String::new(),
            code: STATUS_CODE_ERROR,
        }),
        ..Default::default()
    }
}

fn backend_attributes(record: &Record) -> Vec<KeyValue> {
    let mut attrs = Vec::with_capacity(6);
    if let Some(method) = &record.method {
        attrs.push(KeyValue::string("http.request.method", method));
    }
    if let Some(path) = &record.path {
        attrs.push(KeyValue::string("url.path", path));
    }
    if let Some(status) = record.status {
        attrs.push(KeyValue::int("http.response.status_code", status));
    }
    if let Some(backend) = &record.backend {
        attrs.push(KeyValue::string("varnish.backend", backend));
    }
    if let Some(ttfb) = record.ttfb {
        attrs.push(KeyValue::double("varnish.time_to_first_byte", ttfb));
    }
    if let Some(vxid) = record.vxid {
        attrs.push(KeyValue::int("varnish.vxid", vxid));
    }
    attrs
}

fn server_attributes(record: &Record, include_query: bool) -> Vec<KeyValue> {
    let mut attrs = Vec::with_capacity(15);
    let mut string = |key: &str, value: &Option<String>| {
        if let Some(v) = value {
            attrs.push(KeyValue::string(key, v));
        }
    };
    string("http.request.method", &record.method);
    string("url.path", &record.path);
    if include_query {
        string("url.query", &record.query);
    }
    let (address, port) = record.host.as_deref().map_or((None, None), split_host);
    if let Some(address) = address {
        attrs.push(KeyValue::string("server.address", address));
    }
    if let Some(port) = port {
        attrs.push(KeyValue::int("server.port", port.into()));
    }
    if let Some(version) = record
        .proto
        .as_deref()
        .and_then(|p| p.strip_prefix("HTTP/"))
    {
        attrs.push(KeyValue::string("network.protocol.version", version));
    }
    if let Some(status) = record.status {
        attrs.push(KeyValue::int("http.response.status_code", status));
    }
    if let Some(bytes) = record.bytes {
        attrs.push(KeyValue::int("http.response.body.size", bytes));
    }
    if let Some(ua) = &record.ua {
        attrs.push(KeyValue::string("user_agent.original", ua));
    }
    if let Some(handling) = &record.handling {
        attrs.push(KeyValue::string("varnish.handling", handling));
    }
    if let Some(ttfb) = record.ttfb {
        attrs.push(KeyValue::double("varnish.time_to_first_byte", ttfb));
    }
    if let Some(vxid) = record.vxid {
        attrs.push(KeyValue::int("varnish.vxid", vxid));
    }

    if let Some(hits) = record.hits {
        attrs.push(KeyValue::int("varnish.hits", hits));
    }
    if let Some(ttl) = record.ttl {
        attrs.push(KeyValue::double("varnish.ttl", ttl));
    }
    if let Some(reason) = &record.reason {
        attrs.push(KeyValue::string("varnish.cache_reason", reason));
    }
    attrs
}

/// Splits a `Host` header value into address and optional port, handling `[ipv6]:port`.
fn split_host(host: &str) -> (Option<&str>, Option<u16>) {
    let (address, port) = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, tail)) => (addr, tail.strip_prefix(':')),
            None => (host, None),
        }
    } else if host.matches(':').count() == 1 {
        host.split_once(':')
            .map_or((host, None), |(a, p)| (a, Some(p)))
    } else {
        (host, None)
    };
    let address = Some(address).filter(|a| !a.is_empty());
    (address, port.and_then(|p| p.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::any_value::Value;

    fn record() -> Record {
        Record {
            trace_id: [1; 16],
            span_id: [2; 8],
            parent_id: Some([3; 8]),
            start_ns: 10,
            end_ns: 20,
            ttfb: Some(0.5),
            method: Some("GET".into()),
            host: Some("example.com:8080".into()),
            path: Some("/p".into()),
            query: Some("a=1".into()),
            proto: Some("HTTP/1.1".into()),
            status: Some(200),
            bytes: Some(12),
            handling: Some("hit".into()),
            vxid: Some(7),
            ua: Some("curl".into()),
            hits: Some(4),
            ttl: Some(119.5),
            reason: Some("static".into()),
            ..Default::default()
        }
    }

    fn backend_record() -> Record {
        Record {
            side: Side::Backend,
            parent_id: Some([3; 8]),
            backend: Some("origin".into()),
            // Client-only fields are ignored for a backend span
            host: Some("example.com".into()),
            handling: Some("miss".into()),
            hits: Some(1),
            ..record()
        }
    }

    fn attr<'a>(span: &'a Span, key: &str) -> Option<&'a Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref()?.value.as_ref())
    }

    #[test]
    fn maps_fields_and_attributes() {
        let span = to_span(&record(), false);
        assert_eq!(span.name, "GET");
        assert_eq!(span.kind, 2);
        assert_eq!(span.parent_span_id, vec![3; 8]);
        assert_eq!(
            (span.start_time_unix_nano, span.end_time_unix_nano),
            (10, 20)
        );
        assert_eq!(span.flags, 0);
        assert!(span.status.is_none());
        assert_eq!(
            attr(&span, "http.request.method"),
            Some(&Value::String("GET".into()))
        );
        assert_eq!(attr(&span, "url.path"), Some(&Value::String("/p".into())));
        assert_eq!(
            attr(&span, "server.address"),
            Some(&Value::String("example.com".into()))
        );
        assert_eq!(attr(&span, "server.port"), Some(&Value::Int(8080)));
        assert_eq!(
            attr(&span, "network.protocol.version"),
            Some(&Value::String("1.1".into()))
        );
        assert_eq!(
            attr(&span, "http.response.status_code"),
            Some(&Value::Int(200))
        );
        assert_eq!(
            attr(&span, "http.response.body.size"),
            Some(&Value::Int(12))
        );
        assert_eq!(
            attr(&span, "user_agent.original"),
            Some(&Value::String("curl".into()))
        );
        assert_eq!(
            attr(&span, "varnish.handling"),
            Some(&Value::String("hit".into()))
        );
        assert_eq!(
            attr(&span, "varnish.time_to_first_byte"),
            Some(&Value::Double(0.5))
        );
        assert_eq!(attr(&span, "varnish.vxid"), Some(&Value::Int(7)));
    }

    #[test]
    fn cache_attributes_are_exported_on_the_server_span() {
        let span = to_span(&record(), false);
        assert_eq!(attr(&span, "varnish.hits"), Some(&Value::Int(4)));
        assert_eq!(attr(&span, "varnish.ttl"), Some(&Value::Double(119.5)));
        assert_eq!(
            attr(&span, "varnish.cache_reason"),
            Some(&Value::String("static".into()))
        );
        let bare = Record {
            hits: None,
            ttl: None,
            reason: None,
            ..record()
        };
        let span = to_span(&bare, false);
        for key in ["varnish.hits", "varnish.ttl", "varnish.cache_reason"] {
            assert!(attr(&span, key).is_none(), "{key}");
        }
    }

    #[test]
    fn backend_record_becomes_a_client_span() {
        let span = to_span(&backend_record(), true);
        assert_eq!(span.kind, SPAN_KIND_CLIENT);
        assert_eq!(span.name, "GET");
        assert_eq!(span.parent_span_id, vec![3; 8]);
        assert_eq!(span.span_id, vec![2; 8]);
        assert!(span.status.is_none());
        let keys: Vec<_> = span.attributes.iter().map(|kv| kv.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "http.request.method",
                "url.path",
                "http.response.status_code",
                "varnish.backend",
                "varnish.time_to_first_byte",
                "varnish.vxid"
            ]
        );
        assert_eq!(
            attr(&span, "varnish.backend"),
            Some(&Value::String("origin".into()))
        );
        assert_eq!(
            attr(&span, "varnish.time_to_first_byte"),
            Some(&Value::Double(0.5))
        );
    }

    #[test]
    fn backend_status_is_error_for_5xx_and_fetch_errors() {
        let mut r = backend_record();
        r.status = Some(502);
        assert_eq!(to_span(&r, false).status.unwrap().code, 2);
        r.status = Some(200);
        r.fetch_error = true;
        assert_eq!(to_span(&r, false).status.unwrap().code, 2);
        r.status = Some(404);
        r.fetch_error = false;
        assert!(to_span(&r, false).status.is_none());
    }

    #[test]
    fn query_only_when_enabled() {
        assert!(attr(&to_span(&record(), false), "url.query").is_none());
        assert_eq!(
            attr(&to_span(&record(), true), "url.query"),
            Some(&Value::String("a=1".into()))
        );
    }

    #[test]
    fn parent_absent_is_empty() {
        let mut r = record();
        r.parent_id = None;
        assert!(to_span(&r, false).parent_span_id.is_empty());
    }

    #[test]
    fn status_error_from_500() {
        let mut r = record();
        r.status = Some(503);
        assert_eq!(to_span(&r, false).status.unwrap().code, 2);
        r.status = Some(499);
        assert!(to_span(&r, false).status.is_none());
    }

    #[test]
    fn host_splitting() {
        assert_eq!(split_host("example.com"), (Some("example.com"), None));
        assert_eq!(
            split_host("example.com:80"),
            (Some("example.com"), Some(80))
        );
        assert_eq!(split_host("[::1]:8080"), (Some("::1"), Some(8080)));
        assert_eq!(split_host("[::1]"), (Some("::1"), None));
        assert_eq!(split_host("::1"), (Some("::1"), None));
        assert_eq!(split_host("host:notaport"), (Some("host"), None));
    }

    #[test]
    fn long_strings_are_truncated_on_a_char_boundary() {
        let mut r = record();
        r.path = Some(format!("/{}", "é".repeat(2000)));
        r.method = Some("M".repeat(5000));
        r.reason = Some("é".repeat(2000));
        let span = to_span(&r, false);
        let Some(Value::String(path)) = attr(&span, "url.path") else {
            panic!("url.path missing");
        };
        assert_eq!(path.len(), 1023);
        assert!(path.starts_with("/é"));
        assert_eq!(span.name.len(), 1024);
        let Some(Value::String(reason)) = attr(&span, "varnish.cache_reason") else {
            panic!("varnish.cache_reason missing");
        };
        assert_eq!(reason.len(), 1024);
    }

    #[test]
    fn absent_fields_are_omitted() {
        let span = to_span(
            &Record {
                trace_id: [1; 16],
                span_id: [2; 8],
                ..Default::default()
            },
            true,
        );
        assert!(span.attributes.is_empty());
        assert_eq!(span.name, "HTTP");
    }
}
