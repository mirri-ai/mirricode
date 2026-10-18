//! Shared wire types for the black-box test harness.
//!
//! Every type is the client-side view of the `{code, msg, data, request_id}`
//! envelope's `data` payload, matching the camelCase wire convention of the
//! TypeScript server and the Rust `mirri-server` (`session.create` returns
//! `createdAt`/`updatedAt`/`agentCount`, `meta` returns
//! `serverId`/`startedAt`, ...).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One session as returned by `session.list` and `session.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    /// RFC 3339 timestamp of session creation, as sent by the server.
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification, as sent by the server.
    pub updated_at: String,
    pub agent_count: usize,
    pub tags: Vec<String>,
    /// Session lifecycle status ("running" or "closed"), as sent by the
    /// server. Optional for backward compatibility with older servers.
    #[serde(default)]
    pub status: Option<String>,
}

/// A session agent as embedded in [`SessionDetails`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub id: String,
}

/// Full session details as returned by `session.get`: the summary fields of
/// [`SessionSummary`] plus the session's live agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetails {
    pub id: String,
    pub title: String,
    /// RFC 3339 timestamp of session creation, as sent by the server.
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification, as sent by the server.
    pub updated_at: String,
    pub agent_count: usize,
    pub tags: Vec<String>,
    pub agents: Vec<AgentSummary>,
}

/// Token accounting for one engine turn (`usage` in the `message.send`
/// response).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The engine turn summary returned by `message.send` (the `turn` field of
/// the response `data`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResult {
    pub turn_id: u64,
    /// Normalized stop reason (`"end_turn"`, `"max_tokens"`, `"aborted"`, ...).
    pub stop_reason: String,
    pub steps: u32,
    pub usage: TokenUsage,
    /// Wall-clock duration of the turn in milliseconds.
    pub duration_ms: u64,
}

/// One tool available on a session's agents, as returned by `tool.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: Value,
    /// Whether the tool is deferred (progressive disclosure marker).
    pub deferred: bool,
}

/// Server identity reported by `GET /api/v1/meta` — the v1 REST contract
/// (`metaResponseSchema` in `@mirri-ai/protocol`, also served by the TS
/// kap-server) is snake_case: `server_version` / `server_id` / `started_at`
/// inside the `{code, msg, data}` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerMeta {
    pub server_version: String,
    pub server_id: String,
    /// RFC 3339 timestamp of server start, as sent by the server.
    pub started_at: String,
}

/// Liveness/readiness probe result from `GET /api/v1/healthz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    pub ok: bool,
    pub ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<T: serde::de::DeserializeOwned>(json: &str) -> T {
        serde_json::from_str(json).expect("wire payload deserializes")
    }

    #[test]
    fn should_parse_session_summary_when_given_camel_case_wire_fields() {
        let wire = r#"{
            "id": "session-1",
            "title": "My Session",
            "createdAt": "2025-01-01T00:00:00Z",
            "updatedAt": "2025-01-02T00:00:00Z",
            "agentCount": 2,
            "tags": ["dev", "test"]
        }"#;
        let summary: SessionSummary = parse(wire);
        assert_eq!(summary.id, "session-1");
        assert_eq!(summary.title, "My Session");
        assert_eq!(summary.agent_count, 2);
        assert_eq!(summary.tags, vec!["dev", "test"]);
    }

    #[test]
    fn should_parse_summary_and_agents_when_deserializing_session_details() {
        let wire = r#"{
            "id": "session-1",
            "title": "My Session",
            "createdAt": "2025-01-01T00:00:00Z",
            "updatedAt": "2025-01-02T00:00:00Z",
            "agentCount": 1,
            "tags": [],
            "agents": [{"id": "agent-1"}]
        }"#;
        let details: SessionDetails = parse(wire);
        assert_eq!(details.id, "session-1");
        assert_eq!(details.agent_count, 1);
        assert_eq!(details.agents.len(), 1);
        assert_eq!(details.agents[0].id, "agent-1");
    }

    #[test]
    fn should_parse_turn_payload_when_deserializing_send_message_result() {
        let wire = r#"{
            "turnId": 7,
            "stopReason": "end_turn",
            "steps": 2,
            "usage": {"inputTokens": 10, "outputTokens": 20},
            "durationMs": 15
        }"#;
        let result: SendMessageResult = parse(wire);
        assert_eq!(result.turn_id, 7);
        assert_eq!(result.stop_reason, "end_turn");
        assert_eq!(result.steps, 2);
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 20);
        assert_eq!(result.duration_ms, 15);
    }

    #[test]
    fn should_parse_snake_case_meta_fields_when_deserializing_server_meta() {
        // The v1 REST contract (`metaResponseSchema`) spells the meta
        // payload snake_case — `server_version` (never a camelCase
        // `version`), matching the TS kap-server byte-for-byte.
        let wire = r#"{
            "server_version": "0.1.0-test",
            "server_id": "abc-123",
            "started_at": "2025-01-01T00:00:00Z"
        }"#;
        let meta: ServerMeta = parse(wire);
        assert_eq!(meta.server_version, "0.1.0-test");
        assert_eq!(meta.server_id, "abc-123");
        assert!(!meta.started_at.is_empty());
    }
}
