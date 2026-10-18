//! Client-side frame parsing for inbound server frames.
//!
//! The unified protocol's strict codec ([`JsonRpcFrame::parse`],
//! `mirri-transport`) is deliberately **server-strict**: it rejects any
//! frame that is not a single JSON object, including JSON-RPC 2.0 batch
//! arrays. That strictness is the server's contract (a server never accepts
//! batch requests). The *client* faces the opposite traffic: the server may
//! coalesce runs of consecutive notifications into one array frame
//! (the `eventBatching` capability advertised in `server_hello`, see
//! `mirri-server/src/ws_rpc.rs`). The client therefore flattens a top-level
//! array into its members and parses **each** member through the strict
//! codec — the strict codec itself stays untouched.
//!
//! [`parse_client_frames`] is the only place this flattening lives.

use mirri_transport::frame::{FrameError, JsonRpcFrame};

/// Parse one inbound text frame into zero or more JSON-RPC frames.
///
/// A top-level object parses directly through the strict server codec. A
/// top-level **array** (a batched run of notifications) is flattened: every
/// member is parsed through the same strict codec and collected in order.
/// Anything else — non-JSON text, scalar values, structurally invalid
/// frames — returns the underlying [`FrameError`] so the caller can log and
/// drop it (a client never answers malformed inbound frames; it only has
/// requests/responses to correlate, and protocol noise is dropped).
pub fn parse_client_frames(text: &str) -> Result<Vec<JsonRpcFrame>, FrameError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| FrameError {
        code: mirri_transport::PARSE_ERROR,
        message: format!("parse error: {error}"),
        id: None,
        respond: true,
    })?;
    match value {
        serde_json::Value::Array(members) => {
            let mut frames = Vec::with_capacity(members.len());
            for member in members {
                // Re-serialize the member so the strict codec applies its
                // text-level validation exactly as for a single frame.
                frames.push(JsonRpcFrame::parse(&member.to_string())?);
            }
            Ok(frames)
        }
        _ => Ok(vec![JsonRpcFrame::parse(text)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_flatten_array_into_member_frames_when_server_batches_notifications() {
        // Given an array frame coalescing two event notifications (the
        // server's `eventBatching` form)
        let text = r#"[
            {"jsonrpc":"2.0","method":"event.turn.started","params":{"sessionId":"s","volatile":false}},
            {"jsonrpc":"2.0","method":"event.assistant.delta","params":{"sessionId":"s","volatile":true,"delta":"hi"}}
        ]"#;

        // When the client parses it
        let frames = parse_client_frames(text).expect("array parses");

        // Then both members come out as notifications, in order
        assert_eq!(frames.len(), 2);
        let JsonRpcFrame::Notification(first) = &frames[0] else {
            panic!("first member must be a notification");
        };
        assert_eq!(first.method, "event.turn.started");
        assert_eq!(first.params["sessionId"], "s");
        let JsonRpcFrame::Notification(second) = &frames[1] else {
            panic!("second member must be a notification");
        };
        assert_eq!(second.method, "event.assistant.delta");
        assert_eq!(second.params["delta"], "hi");
    }

    #[test]
    fn should_parse_single_object_directly_when_frame_is_not_batched() {
        // Given a plain single-object frame (no batching)
        let text = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;

        // When the client parses it
        let frames = parse_client_frames(text).expect("single frame parses");

        // Then it yields exactly one response frame
        assert_eq!(frames.len(), 1);
        let JsonRpcFrame::Response(response) = &frames[0] else {
            panic!("expected a response frame");
        };
        assert_eq!(
            response.id,
            Some(mirri_transport::frame::RequestId::Number(3))
        );
        assert_eq!(
            response.result.as_ref().and_then(|r| r.get("ok")),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn should_report_parse_error_when_text_is_not_valid_json() {
        // Given garbage on the wire
        let text = "{not json";

        // When the client parses it
        let error = parse_client_frames(text).expect_err("invalid JSON fails");

        // Then the error is a parse error (-32700) carrying no recoverable id
        assert_eq!(error.code, mirri_transport::PARSE_ERROR);
        assert_eq!(error.id, None);
        // A client drops malformed inbound frames — `respond` is ignored.
        assert!(error.respond);
    }

    #[test]
    fn should_report_error_when_array_member_is_structurally_invalid() {
        // Given a batch whose first member is fine and whose second member
        // is an id-less response (structurally invalid per the strict codec)
        let text = r#"[
            {"jsonrpc":"2.0","method":"event.x","params":{}},
            {"jsonrpc":"2.0","result":{}}
        ]"#;

        // When the client parses it
        let error = parse_client_frames(text).expect_err("invalid member fails the batch");

        // Then the invalid member's error surfaces (the batch is dropped as
        // protocol noise; the well-formed member is not half-applied)
        assert_eq!(error.code, mirri_transport::INVALID_REQUEST);
    }

    #[test]
    fn should_accept_empty_array_as_an_empty_batch() {
        // An empty array is a degenerate batch carrying zero members; the
        // flatten path yields no frames and the caller simply has nothing to
        // process (matching the TS client, which loops over an empty batch).
        let frames = parse_client_frames("[]").expect("empty array is an empty batch");
        assert!(frames.is_empty());
    }
}
