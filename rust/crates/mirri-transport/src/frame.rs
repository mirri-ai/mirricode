//! JSON-RPC 2.0 frame definitions.
//!
//! These types are shared across all transports. The wire format is
//! identical whether the frame travels over WebSocket, stdio, or FFI.
//!
//! Two entry points, layered:
//!
//! - [`JsonRpcFrame::from_value`] classifies an already-parsed JSON value
//!   leniently (the `jsonrpc` member is tolerated but not required).
//! - [`JsonRpcFrame::parse`] applies the strict text-level validation of the
//!   unified protocol: it rejects non-object frames (including batch
//!   arrays), non-integer ids, frames carrying both `method` and
//!   `result`/`error`, and responses missing a usable id — reporting
//!   failures as [`FrameError`] with JSON-RPC 2.0 error codes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Parse error — the text was not valid JSON (JSON-RPC 2.0 §5.1).
pub const PARSE_ERROR: i64 = -32700;
/// Invalid request — the frame is structurally invalid (JSON-RPC 2.0 §5.2).
pub const INVALID_REQUEST: i64 = -32600;
/// Method not found (JSON-RPC 2.0 §5.3).
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Invalid params — a valid method with invalid arguments (§5.4).
pub const INVALID_PARAMS: i64 = -32602;
/// Internal error (§5.5).
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC 2.0 request/response id — string or number per the spec
/// (§4). ACP clients mint either shape: the official client crates (Zed
/// et al.) send a UUID string on every request, while this codebase's
/// own faces (web server, remote adapter) mint incrementing integers —
/// both ride the same type, and responses echo the request's id
/// verbatim.
///
/// JSON-RPC's `null` id (the unparseable-request sentinel) is not a
/// variant: the frame structs use `Option` — `None` means notification
/// (request) or `id: null` (response).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// The integer id this codebase's own clients mint.
    Number(i64),
    /// The string id external ACP clients mint (Zed: UUID v4).
    Str(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

/// A JSON-RPC 2.0 request frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Request id. `None` means notification (no response expected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// Method name (e.g. `"session.create"`).
    pub method: String,
    /// Method parameters. May be `Null` for no params.
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: Option<i64>, method: impl Into<String>, params: Value) -> Self {
        Self {
            id: id.map(RequestId::Number),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Matches the request id. `None` serializes as `"id": null`, which
    /// JSON-RPC 2.0 requires when a request could not be parsed (`-32700`).
    pub id: Option<RequestId>,
    /// Result on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: impl Into<RequestId>, result: Value) -> Self {
        Self {
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: impl Into<RequestId>, error: JsonRpcError) -> Self {
        Self {
            id: Some(id.into()),
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// A JSON-RPC 2.0 notification (no id, no response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

/// A complete JSON-RPC 2.0 frame — request, response, or notification.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcFrame {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

impl JsonRpcFrame {
    /// The JSON-RPC version declared by every frame on the wire.
    pub const JSONRPC_VERSION: &'static str = "2.0";

    /// Parse a JSON value into a frame.
    ///
    /// The optional `"jsonrpc"` member is tolerated but not required: frames
    /// from older peers may omit it, and serde ignores unknown fields during
    /// deserialization of the inner structs.
    pub fn from_value(value: Value) -> Result<Self, FrameParseError> {
        let obj = value.as_object().ok_or(FrameParseError::NotAnObject)?;

        if obj.contains_key("method") {
            if obj.contains_key("id") {
                // Request with id
                let req: JsonRpcRequest =
                    serde_json::from_value(value).map_err(FrameParseError::Deserialize)?;
                Ok(Self::Request(req))
            } else {
                // Notification (no id)
                let notif: JsonRpcNotification =
                    serde_json::from_value(value).map_err(FrameParseError::Deserialize)?;
                Ok(Self::Notification(notif))
            }
        } else if obj.contains_key("id") {
            // Response
            let resp: JsonRpcResponse =
                serde_json::from_value(value).map_err(FrameParseError::Deserialize)?;
            Ok(Self::Response(resp))
        } else {
            Err(FrameParseError::UnknownFrameType)
        }
    }

    /// Parse one frame from its wire text, applying the strict structural
    /// validation of the unified protocol (design §4).
    ///
    /// Parse errors (`-32700`) are always reported with `respond: true` and
    /// `id: None` — the caller answers with `id: null`. Structurally invalid
    /// frames (`-32600`) are reported with `respond: true` when the frame
    /// carried an `id` (an unechoable id still answers with `id: null`) and
    /// `respond: false` otherwise — an id-less invalid frame is
    /// notification-shaped and dropped silently per JSON-RPC 2.0.
    pub fn parse(text: &str) -> Result<JsonRpcFrame, FrameError> {
        let value: Value = serde_json::from_str(text).map_err(|error| FrameError {
            code: PARSE_ERROR,
            message: format!("parse error: {error}"),
            id: None,
            // Parse errors are always answered, with `id: null`.
            respond: true,
        })?;
        let object = value.as_object().ok_or_else(|| FrameError {
            code: INVALID_REQUEST,
            message: "invalid request: frame must be a single JSON object".to_string(),
            id: None,
            respond: false,
        })?;
        // `has_id_key` (an `id` field is present) and `id` (it is an integer
        // this codec can echo) are tracked separately: a frame carrying an
        // unechoable id (string, null, object) is still answered — with
        // `id: null` — while a frame with no id at all is notification-shaped
        // and dropped.
        let has_id_key = object.contains_key("id");
        let id = object.get("id").and_then(Value::as_i64);
        let has_method = object.contains_key("method");
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");
        let invalid = |message: String| FrameError {
            code: INVALID_REQUEST,
            message,
            id,
            respond: has_id_key,
        };

        match (has_method, has_result, has_error) {
            (true, false, false) => {
                if has_id_key && id.is_none() {
                    // A request whose id is not an integer cannot be echoed;
                    // answer -32600 with `id: null`.
                    return Err(invalid(
                        "invalid request: request ids must be integers".to_string(),
                    ));
                }
                let typed = if has_id_key {
                    serde_json::from_value::<JsonRpcRequest>(value).map(JsonRpcFrame::Request)
                } else {
                    serde_json::from_value::<JsonRpcNotification>(value)
                        .map(JsonRpcFrame::Notification)
                };
                typed.map_err(|error| invalid(format!("invalid request: {error}")))
            }
            (false, result, error) if result ^ error => {
                if !has_id_key || id.is_none() {
                    // A response-shaped frame (a) without an id is an invalid
                    // notification: dropped silently by the caller; (b) with
                    // an unechoable id is answered with `id: null`.
                    return Err(invalid(
                        "invalid response: responses must carry an integer id".to_string(),
                    ));
                }
                serde_json::from_value::<JsonRpcResponse>(value)
                    .map(JsonRpcFrame::Response)
                    .map_err(|error| invalid(format!("invalid response: {error}")))
            }
            _ => Err(invalid(
                "invalid request: expected exactly one of 'method' or 'result'/'error'".to_string(),
            )),
        }
    }

    /// Serialize to JSON value.
    ///
    /// Every frame — request, response, or notification — carries the
    /// mandatory `"jsonrpc": "2.0"` member (JSON-RPC 2.0 §3).
    pub fn to_value(&self) -> Value {
        let mut value = match self {
            Self::Request(r) => serde_json::to_value(r).unwrap_or(Value::Null),
            Self::Response(r) => serde_json::to_value(r).unwrap_or(Value::Null),
            Self::Notification(n) => serde_json::to_value(n).unwrap_or(Value::Null),
        };
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "jsonrpc".to_string(),
                Value::String(Self::JSONRPC_VERSION.to_string()),
            );
        }
        value
    }
}

impl std::fmt::Display for JsonRpcFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_value().to_string())
    }
}

/// Error when parsing a frame from JSON.
#[derive(Debug, thiserror::Error)]
pub enum FrameParseError {
    #[error("frame is not a JSON object")]
    NotAnObject,
    #[error("deserialize error: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("unknown frame type: no 'method' or 'id' field")]
    UnknownFrameType,
}

/// Why [`JsonRpcFrame::parse`] failed. Two error classes, distinguishable by
/// `code`:
///
/// - `-32700` (parse error): the caller must always answer, with `id: null`;
/// - `-32600` (invalid request): the caller answers with the echoed `id`
///   when one is recoverable (`respond` is also true for frames that carried
///   an unechoable id), and drops the frame silently when `respond` is
///   false — an id-less invalid frame, which per JSON-RPC 2.0 is
///   notification-shaped and never answered.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameError {
    pub code: i64,
    pub message: String,
    /// The id recovered from the offending frame, when it was an integer.
    pub id: Option<i64>,
    /// Whether the caller must answer (always true for parse errors; true
    /// for structurally invalid frames that carried an `id`).
    pub respond: bool,
}

/// Build the wire value of a protocol-level error response (`-32700` /
/// `-32600`), which echoes `id` verbatim — including `null` for unrecoverable
/// requests. Use [`JsonRpcResponse::err`] for replies to well-parsed requests.
pub fn protocol_error_frame(code: i64, message: impl Into<String>, id: Option<i64>) -> Value {
    json!({
        "jsonrpc": JsonRpcFrame::JSONRPC_VERSION,
        "error": { "code": code, "message": message.into() },
        "id": id.map(Value::from).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_request_when_frame_has_method_and_id() {
        let v = serde_json::json!({"id": 1, "method": "session.create", "params": {}});
        let frame = JsonRpcFrame::from_value(v).unwrap();
        assert!(matches!(frame, JsonRpcFrame::Request(_)));
    }

    #[test]
    fn should_parse_notification_when_frame_has_method_but_no_id() {
        let v = serde_json::json!({"method": "event", "params": {"type": "assistant.delta"}});
        let frame = JsonRpcFrame::from_value(v).unwrap();
        assert!(matches!(frame, JsonRpcFrame::Notification(_)));
    }

    #[test]
    fn should_parse_response_when_frame_has_id_but_no_method() {
        let v = serde_json::json!({"id": 1, "result": {"ok": true}});
        let frame = JsonRpcFrame::from_value(v).unwrap();
        assert!(matches!(frame, JsonRpcFrame::Response(_)));
    }

    #[test]
    fn should_return_error_when_frame_has_no_method_or_id() {
        let v = serde_json::json!({"foo": "bar"});
        let err = JsonRpcFrame::from_value(v).unwrap_err();
        assert!(matches!(err, FrameParseError::UnknownFrameType));
    }

    #[test]
    fn should_include_jsonrpc_2_0_when_serializing_any_frame() {
        let frames = [
            JsonRpcFrame::Request(JsonRpcRequest {
                id: Some(RequestId::Number(1)),
                method: "session.create".to_string(),
                params: serde_json::json!({}),
            }),
            JsonRpcFrame::Response(JsonRpcResponse {
                id: Some(RequestId::Number(1)),
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            }),
            JsonRpcFrame::Notification(JsonRpcNotification {
                method: "event".to_string(),
                params: serde_json::json!({}),
            }),
        ];
        for frame in frames {
            let value = frame.to_value();
            assert_eq!(
                value["jsonrpc"], "2.0",
                "every frame must declare jsonrpc 2.0 on the wire: {value}"
            );
        }
    }

    #[test]
    fn should_tolerate_jsonrpc_field_when_parsing() {
        // The "jsonrpc" member must not be required...
        let v = serde_json::json!({"id": 1, "method": "session.create", "params": {}});
        assert!(matches!(
            JsonRpcFrame::from_value(v).unwrap(),
            JsonRpcFrame::Request(_)
        ));

        // ...but frames that carry it must still parse.
        let v = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "session.create", "params": {}});
        assert!(matches!(
            JsonRpcFrame::from_value(v).unwrap(),
            JsonRpcFrame::Request(_)
        ));
    }

    #[test]
    fn should_serialize_null_id_for_response_without_request_id() {
        // JSON-RPC 2.0 §4 requires a null id when a request could not be parsed.
        let frame = JsonRpcFrame::Response(JsonRpcResponse {
            id: None,
            result: None,
            error: Some(JsonRpcError {
                code: -32700,
                message: "parse error".to_string(),
                data: None,
            }),
        });
        let value = frame.to_value();
        assert_eq!(value["jsonrpc"], "2.0");
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["code"], -32700);
    }

    // -----------------------------------------------------------------------
    // Strict text-level parse (ported from mirri-server's json_rpc.rs)
    // -----------------------------------------------------------------------

    #[test]
    fn should_parse_request_when_the_id_is_a_string() {
        // JSON-RPC 2.0 §4 allows string or number ids, and ACP clients
        // (Zed via the official client crate) mint a UUID string for
        // every request — the lenient value path must keep them.
        let v = serde_json::json!({"id": "req-1", "method": "session.create", "params": {}});
        let JsonRpcFrame::Request(request) =
            JsonRpcFrame::from_value(v).expect("a string-id request parses")
        else {
            panic!("expected a request frame");
        };
        assert_eq!(request.id, Some(RequestId::Str("req-1".to_string())));
        assert_eq!(request.method, "session.create");
    }

    #[test]
    fn should_serialize_a_string_response_id_verbatim_and_round_trip_it() {
        // The response must echo the request's string id back untouched.
        let frame = JsonRpcFrame::Response(JsonRpcResponse {
            id: Some(RequestId::Str("req-1".to_string())),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        });
        let value = frame.to_value();
        assert_eq!(value["id"], json!("req-1"));

        let JsonRpcFrame::Response(reparsed) =
            JsonRpcFrame::from_value(value).expect("the response round-trips")
        else {
            panic!("expected a response frame");
        };
        assert_eq!(reparsed.id, Some(RequestId::Str("req-1".to_string())));
    }

    #[test]
    fn should_parse_request_with_integer_id_when_parsing_wire_text() {
        let frame = JsonRpcFrame::parse(
            r#"{"jsonrpc":"2.0","id":7,"method":"session.create","params":{"title":"t"}}"#,
        )
        .expect("request parses");
        let JsonRpcFrame::Request(request) = frame else {
            panic!("expected a request frame, got {frame:?}");
        };
        assert_eq!(request.id, Some(RequestId::Number(7)));
        assert_eq!(request.method, "session.create");
        assert_eq!(request.params, json!({ "title": "t" }));
    }

    #[test]
    fn should_parse_request_when_jsonrpc_marker_missing_and_params_null() {
        // The `jsonrpc` field is optional on input; params may be omitted.
        let frame = JsonRpcFrame::parse(r#"{"id":1,"method":"session.list"}"#).expect("parses");
        let JsonRpcFrame::Request(request) = frame else {
            panic!("expected a request frame");
        };
        assert_eq!(request.id, Some(RequestId::Number(1)));
        assert_eq!(request.params, Value::Null);
    }

    #[test]
    fn should_parse_notification_when_frame_has_no_id() {
        let frame = JsonRpcFrame::parse(r#"{"jsonrpc":"2.0","method":"event","params":{"a":1}}"#)
            .expect("notification parses");
        let JsonRpcFrame::Notification(notification) = frame else {
            panic!("expected a notification frame");
        };
        assert_eq!(notification.method, "event");
        assert_eq!(notification.params, json!({ "a": 1 }));
    }

    #[test]
    fn should_parse_success_and_error_responses_from_wire_text() {
        let JsonRpcFrame::Response(ok) =
            JsonRpcFrame::parse(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#)
                .expect("success response parses")
        else {
            panic!("expected a response frame");
        };
        assert_eq!(ok, JsonRpcResponse::ok(3, json!({ "ok": true })));

        let JsonRpcFrame::Response(err) = JsonRpcFrame::parse(
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"nope"}}"#,
        )
        .expect("error response parses") else {
            panic!("expected a response frame");
        };
        assert_eq!(
            err,
            JsonRpcResponse::err(4, JsonRpcError::new(-32601, "nope"))
        );
    }

    #[test]
    fn should_report_parse_error_when_text_is_not_valid_json() {
        let error = JsonRpcFrame::parse("{not json").expect_err("invalid JSON fails");
        assert_eq!(error.code, PARSE_ERROR);
        assert_eq!(error.id, None);
        assert!(error.respond, "parse errors are always answered");
    }

    #[test]
    fn should_report_invalid_request_when_method_missing_but_id_present() {
        let error = JsonRpcFrame::parse(r#"{"jsonrpc":"2.0","id":1}"#).expect_err("invalid");
        assert_eq!(error.code, INVALID_REQUEST);
        assert_eq!(error.id, Some(1));
        assert!(error.respond, "frames that carried an id are answered");
    }

    #[test]
    fn should_drop_invalid_frames_when_no_id_is_present() {
        // A structurally invalid frame without an id must not be answered.
        let error = JsonRpcFrame::parse(r#"{"jsonrpc":"2.0","result":{}}"#)
            .expect_err("id-less response is invalid");
        assert_eq!(error.code, INVALID_REQUEST);
        assert_eq!(error.id, None);
        assert!(
            !error.respond,
            "id-less invalid frames are dropped silently"
        );
    }

    #[test]
    fn should_reject_frames_when_both_result_and_error_are_present() {
        let error = JsonRpcFrame::parse(
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"m"}}"#,
        )
        .expect_err("both result and error is invalid");
        assert_eq!(error.code, INVALID_REQUEST);
        assert_eq!(error.id, Some(1));
        assert!(error.respond, "frames that carried an id are answered");
    }

    #[test]
    fn should_reject_requests_when_their_id_is_not_an_integer() {
        // The codec uses integer ids; a string-id request cannot be
        // answered correctly, so it is a -32600 with no echoable id.
        let error = JsonRpcFrame::parse(r#"{"id":"r-1","method":"session.list"}"#)
            .expect_err("string id is invalid");
        assert_eq!(error.code, INVALID_REQUEST);
        assert_eq!(error.id, None);
        assert!(
            error.respond,
            "an unechoable id is still answered — with id: null"
        );
    }

    #[test]
    fn should_reject_non_object_frames_when_parsing_wire_text() {
        let error = JsonRpcFrame::parse(r#"[{"id":1,"method":"session.list"}]"#)
            .expect_err("batch frames are not accepted from clients");
        assert_eq!(error.code, INVALID_REQUEST);
        assert!(
            !error.respond,
            "a batch array carries no recoverable id and is dropped"
        );
    }

    #[test]
    fn should_stamp_jsonrpc_marker_when_serializing_frames() {
        let value = JsonRpcFrame::Notification(JsonRpcNotification::new(
            "server_hello",
            json!({ "serverId": "s" }),
        ))
        .to_value();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "server_hello");

        let response =
            JsonRpcFrame::Response(JsonRpcResponse::ok(1, json!({ "ok": true }))).to_value();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"], json!({ "ok": true }));
        assert!(
            response.get("error").is_none(),
            "empty error must not be emitted"
        );
    }

    #[test]
    fn should_use_null_id_when_building_protocol_error_frames() {
        let frame = protocol_error_frame(PARSE_ERROR, "bad json", None);
        assert_eq!(frame["id"], Value::Null);
        assert_eq!(frame["error"]["code"], PARSE_ERROR);

        let frame = protocol_error_frame(INVALID_REQUEST, "bad shape", Some(9));
        assert_eq!(frame["id"], 9);
    }

    #[test]
    fn should_round_trip_all_frame_kinds_through_display() {
        for frame in [
            JsonRpcFrame::Request(JsonRpcRequest::new(Some(1), "session.get", json!({}))),
            JsonRpcFrame::Notification(JsonRpcNotification::new("ping", json!({ "nonce": "n" }))),
            JsonRpcFrame::Response(JsonRpcResponse::err(2, JsonRpcError::new(-32601, "x"))),
        ] {
            let text = frame.to_string();
            let reparsed = JsonRpcFrame::parse(&text).expect("round trip parses");
            assert_eq!(reparsed, frame);
        }
    }
}
