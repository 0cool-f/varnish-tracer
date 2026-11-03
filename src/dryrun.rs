//! One-line JSON rendering of a span for `--dry-run`.

use crate::otlp::{SPAN_KIND_CLIENT, SPAN_KIND_SERVER, STATUS_CODE_ERROR, Span, any_value::Value};
use serde_json::{Map, json};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ids are hex, times are unix nanoseconds, `parent_span_id` is empty for a root span and `status` is null unless it is an error.
pub fn json_line(span: &Span) -> String {
    let attributes: Map<String, serde_json::Value> = span
        .attributes
        .iter()
        .filter_map(|kv| {
            let value = match kv.value.as_ref()?.value.as_ref()? {
                Value::String(v) => json!(v),
                Value::Bool(v) => json!(v),
                Value::Int(v) => json!(v),
                Value::Double(v) => json!(v),
            };
            Some((kv.key.clone(), value))
        })
        .collect();
    let kind = match span.kind {
        SPAN_KIND_SERVER => "SERVER",
        SPAN_KIND_CLIENT => "CLIENT",
        _ => "UNSPECIFIED",
    };
    let status = span.status.as_ref().map(|status| match status.code {
        STATUS_CODE_ERROR => json!({"code": "ERROR"}),
        _ => json!({"code": "UNSET"}),
    });
    json!({
        "trace_id": hex(&span.trace_id),
        "span_id": hex(&span.span_id),
        "parent_span_id": hex(&span.parent_span_id),
        "name": span.name,
        "kind": kind,
        "start_time_unix_nano": span.start_time_unix_nano,
        "end_time_unix_nano": span.end_time_unix_nano,
        "attributes": attributes,
        "status": status,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::{KeyValue, Status};

    fn span() -> Span {
        Span {
            trace_id: vec![0x0a; 16],
            span_id: vec![0xb7; 8],
            parent_span_id: vec![0x00, 0xf0, 0, 0, 0, 0, 0, 0x01],
            name: "GET".into(),
            kind: SPAN_KIND_SERVER,
            start_time_unix_nano: 1_793_455_717_323_367_000,
            end_time_unix_nano: 1_793_455_717_323_545_000,
            attributes: vec![
                KeyValue::string("url.path", "/a\"b\n"),
                KeyValue::int("http.response.status_code", 200),
                KeyValue::double("varnish.ttl", 12.5),
            ],
            status: Some(Status {
                message: String::new(),
                code: STATUS_CODE_ERROR,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn shape_of_the_line() {
        let line = json_line(&span());
        assert!(!line.contains('\n'));
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json["trace_id"], "0a".repeat(16));
        assert_eq!(json["span_id"], "b7".repeat(8));
        assert_eq!(json["parent_span_id"], "00f0000000000001");
        assert_eq!(json["name"], "GET");
        assert_eq!(json["kind"], "SERVER");
        assert_eq!(json["start_time_unix_nano"], 1_793_455_717_323_367_000u64);
        assert_eq!(json["end_time_unix_nano"], 1_793_455_717_323_545_000u64);
        assert_eq!(json["attributes"]["url.path"], "/a\"b\n");
        assert_eq!(json["attributes"]["http.response.status_code"], 200);
        assert_eq!(json["attributes"]["varnish.ttl"], 12.5);
        assert_eq!(json["status"]["code"], "ERROR");
    }

    #[test]
    fn root_client_span_without_status() {
        let line = json_line(&Span {
            kind: SPAN_KIND_CLIENT,
            status: None,
            parent_span_id: vec![],
            ..span()
        });
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json["kind"], "CLIENT");
        assert_eq!(json["parent_span_id"], "");
        assert!(json["status"].is_null());
    }
}
