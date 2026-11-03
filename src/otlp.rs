//! Minimal OTLP trace protocol messages; field numbers follow opentelemetry-proto.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct ExportTraceServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResourceSpans {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_spans: Vec<ScopeSpans>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "2")]
    pub dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ScopeSpans {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub spans: Vec<Span>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExportTraceServiceResponse {
    #[prost(message, optional, tag = "1")]
    pub partial_success: Option<ExportTracePartialSuccess>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExportTracePartialSuccess {
    #[prost(int64, tag = "1")]
    pub rejected_spans: i64,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

pub const SPAN_KIND_SERVER: i32 = 2;
pub const SPAN_KIND_CLIENT: i32 = 3;
pub const STATUS_CODE_ERROR: i32 = 2;

#[derive(Clone, PartialEq, Message)]
pub struct Span {
    #[prost(bytes = "vec", tag = "1")]
    pub trace_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub span_id: Vec<u8>,
    #[prost(string, tag = "3")]
    pub trace_state: String,
    #[prost(bytes = "vec", tag = "4")]
    pub parent_span_id: Vec<u8>,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(int32, tag = "6")]
    pub kind: i32,
    #[prost(fixed64, tag = "7")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "8")]
    pub end_time_unix_nano: u64,
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
    #[prost(message, optional, tag = "15")]
    pub status: Option<Status>,
    #[prost(fixed32, tag = "16")]
    pub flags: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Status {
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(int32, tag = "3")]
    pub code: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        String(String),
        #[prost(bool, tag = "2")]
        Bool(bool),
        #[prost(int64, tag = "3")]
        Int(i64),
        #[prost(double, tag = "4")]
        Double(f64),
    }
}

impl KeyValue {
    pub fn string(key: &str, value: &str) -> Self {
        Self::new(
            key,
            any_value::Value::String(
                crate::text::truncate(value, crate::text::MAX_VALUE_BYTES).to_owned(),
            ),
        )
    }

    pub fn int(key: &str, value: i64) -> Self {
        Self::new(key, any_value::Value::Int(value))
    }

    pub fn double(key: &str, value: f64) -> Self {
        Self::new(key, any_value::Value::Double(value))
    }

    fn new(key: &str, value: any_value::Value) -> Self {
        Self {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent decoder shapes with the same field numbers, to check the wire layout.
    #[derive(Clone, PartialEq, Message)]
    struct Req2 {
        #[prost(message, repeated, tag = "1")]
        rs: Vec<Rs2>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Rs2 {
        #[prost(message, repeated, tag = "2")]
        ss: Vec<Ss2>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Ss2 {
        #[prost(message, repeated, tag = "2")]
        spans: Vec<Sp2>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct Sp2 {
        #[prost(bytes = "vec", tag = "1")]
        trace: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        span: Vec<u8>,
        #[prost(string, tag = "5")]
        name: String,
        #[prost(int32, tag = "6")]
        kind: i32,
        #[prost(fixed64, tag = "7")]
        start: u64,
        #[prost(fixed64, tag = "8")]
        end: u64,
        #[prost(message, optional, tag = "15")]
        status: Option<St2>,
    }
    #[derive(Clone, PartialEq, Message)]
    struct St2 {
        #[prost(int32, tag = "3")]
        code: i32,
    }

    fn sample_span() -> Span {
        Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            name: "GET".into(),
            kind: SPAN_KIND_SERVER,
            start_time_unix_nano: 1_793_455_717_323_367_000,
            end_time_unix_nano: 1_793_455_717_323_545_000,
            attributes: vec![KeyValue::int("http.response.status_code", 500)],
            status: Some(Status {
                message: String::new(),
                code: STATUS_CODE_ERROR,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn span_wire_tags() {
        let bytes = sample_span().encode_to_vec();
        assert_eq!(&bytes[..2], &[0x0a, 16]);
        let start_tag = bytes.iter().position(|&b| b == 0x39).unwrap();
        assert_eq!(
            bytes[start_tag + 1..start_tag + 9],
            1_793_455_717_323_367_000u64.to_le_bytes()
        );
        assert_eq!(bytes[start_tag + 9], 0x41);
    }

    #[test]
    fn request_decodes_with_independent_structs() {
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource::default()),
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans: vec![sample_span()],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };
        let decoded = Req2::decode(req.encode_to_vec().as_slice()).unwrap();
        let span = &decoded.rs[0].ss[0].spans[0];
        assert_eq!(span.trace, vec![1; 16]);
        assert_eq!(span.span, vec![2; 8]);
        assert_eq!(span.name, "GET");
        assert_eq!(span.kind, 2);
        assert_eq!(span.start, 1_793_455_717_323_367_000);
        assert_eq!(span.end, 1_793_455_717_323_545_000);
        assert_eq!(span.status.as_ref().unwrap().code, 2);
    }

    #[test]
    fn any_value_tags() {
        let kv = KeyValue::double("k", 1.5);
        let bytes = kv.encode_to_vec();
        assert_eq!(
            bytes,
            [0x0a, 1, b'k', 0x12, 9, 0x21, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f]
        );
    }
}
