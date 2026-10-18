//! `question.ask` bridging — the engine's `AskUserQuestion` reverse RPC
//! mapped onto the ACP client's interactive surfaces (F-P5b-1).
//!
//! ACP has no dedicated question method, so a question ask takes one of
//! two roads, exactly like the TS host's `interaction-bridge.ts`:
//!
//! 1. **Clients that advertised `clientCapabilities.elicitation.form`**
//!    at `initialize` receive an `elicitation/create` request in form
//!    mode — every question natively (single-select → a `string` +
//!    `oneOf` schema property, multi-select → an `array` + `items.anyOf`
//!    with `minItems: 1`).
//! 2. **Everyone else degrades** to `session/request_permission` with a
//!    single-select surface: one `allow_once` option per declared label
//!    (ids in a `q0_opt_*` namespace so the round-trip is unambiguous)
//!    plus a trailing `reject_once` "Skip" — and only the *first*
//!    question is asked (the degradation rule the TS bridge logs).
//!
//! The engine-side reply is the rust-native question-answer shape
//! `{"answers": {question: answer}}` (the same wire form
//! `interaction.respond` / the node-sdk e2e responder use); a dismissed
//! question (decline / cancel / skip / malformed reply) answers with the
//! empty map — no fabricated answers, never a hang.

use serde_json::{Value, json};

/// Whether the client's advertised `clientCapabilities` include
/// `elicitation.form` (the form-mode question surface).
pub(crate) fn elicitation_form_supported(client_capabilities: Option<&Value>) -> bool {
    client_capabilities
        .and_then(|capabilities| capabilities.get("elicitation"))
        .and_then(|elicitation| elicitation.get("form"))
        .is_some_and(|form| !form.is_null())
}

/// Map a `question.ask` payload's `questions` array into an
/// `elicitation/create` form-mode request (TS
/// `questionRequestToElicitationParams`).
///
/// Each question `i` becomes the property `q{i}`: titled by the question's
/// `header` (falling back to the full question text), described by its
/// `body`, with one titled enum option per declared label. A
/// multi-select question becomes an `array` with `minItems: 1`; every
/// property is required.
pub(crate) fn question_to_elicitation_params(session_id: &str, questions: &[Value]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let key = format!("q{index}");
        required.push(json!(key));
        let title = question
            .get("header")
            .and_then(Value::as_str)
            .filter(|header| !header.is_empty())
            .or_else(|| question.get("question").and_then(Value::as_str))
            .unwrap_or_default();
        let mut property = json!({
            "title": title,
            "oneOf": titled_enum_options(question),
        });
        if let Some(body) = question.get("body").and_then(Value::as_str)
            && !body.is_empty()
        {
            property["description"] = json!(body);
        }
        if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
            let options = titled_enum_options(question);
            property.as_object_mut().unwrap().remove("oneOf");
            property["type"] = json!("array");
            property["minItems"] = json!(1);
            property["items"] = json!({"anyOf": options});
        } else {
            property["type"] = json!("string");
        }
        properties.insert(key, property);
    }
    json!({
        "sessionId": session_id,
        "mode": "form",
        "message": questions
            .iter()
            .map(|question| question.get("question").and_then(Value::as_str).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        "requestedSchema": {"type": "object", "properties": properties, "required": required},
    })
}

/// The titled enum options of one question's declared labels (shared by
/// the single-select `oneOf` and multi-select `items.anyOf` arms).
fn titled_enum_options(question: &Value) -> Value {
    Value::Array(
        question
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .map(|option| {
                        let label = option
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let mut entry = json!({"const": label, "title": label});
                        if let Some(description) = option.get("description").and_then(Value::as_str)
                            && !description.is_empty()
                        {
                            entry["description"] = json!(description);
                        }
                        entry
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Map the client's `elicitation/create` response back into the
/// engine-side answer payload `{"answers": {question: answer}}` (TS
/// `elicitationResponseToQuestionAnswers`): `accept` with usable content
/// answers; decline / cancel / an accept without content / values outside
/// the declared options resolve to the dismissed empty map.
pub(crate) fn elicitation_response_to_answers(questions: &[Value], response: &Value) -> Value {
    let mut answers = serde_json::Map::new();
    if response.get("action").and_then(Value::as_str) == Some("accept")
        && let Some(content) = response.get("content").filter(|c| c.is_object())
    {
        for (index, question) in questions.iter().enumerate() {
            let Some(value) = content.get(format!("q{index}")) else {
                continue;
            };
            let question_text = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let declared_labels: Vec<&str> = question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .map(|option| option.get("label").and_then(Value::as_str).unwrap_or(""))
                        .collect()
                })
                .unwrap_or_default();
            if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                let Some(picked) = value.as_array() else {
                    continue;
                };
                let selected: Vec<&str> = picked
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|label| declared_labels.contains(label))
                    .collect();
                // Join in DECLARED option order (TUI QuestionDialog
                // encoding parity).
                let ordered: Vec<&str> = declared_labels
                    .iter()
                    .filter(|label| selected.contains(label))
                    .copied()
                    .collect();
                if !ordered.is_empty() {
                    answers.insert(question_text.to_string(), json!(ordered.join(", ")));
                }
            } else if let Some(answer) = value.as_str()
                && declared_labels.contains(&answer)
            {
                answers.insert(question_text.to_string(), json!(answer));
            }
        }
    }
    json!({"answers": answers})
}

/// Map a `question.ask` payload into the degraded
/// `session/request_permission` request: the FIRST question as a
/// single-select surface, one `allow_once` option per label plus a
/// trailing `reject_once` "Skip" (TS `questionItemToPermissionOptions`,
/// `q0_opt_*` id namespace).
pub(crate) fn question_to_permission_params(session_id: &str, questions: &[Value]) -> Value {
    let question = questions
        .first()
        .cloned()
        .unwrap_or_else(|| json!({"question": "", "options": []}));
    let question_text = question
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut options: Vec<Value> = question
        .get("options")
        .and_then(Value::as_array)
        .map(|declared| {
            declared
                .iter()
                .enumerate()
                .map(|(option_index, option)| {
                    json!({
                        "optionId": format!("q0_opt_{option_index}"),
                        "name": option.get("label").and_then(Value::as_str).unwrap_or_default(),
                        "kind": "allow_once",
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    options.push(json!({
        "optionId": "q0_skip",
        "name": "Skip",
        "kind": "reject_once",
    }));
    json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": "ask-user",
            "title": "AskUserQuestion",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": question_text},
            }],
        },
        "options": options,
    })
}

/// Map the client's degraded `session/request_permission` response back
/// into the engine-side answer payload (TS `outcomeToQuestionAnswer`):
/// a `q0_opt_<i>` selection answers with that label; skip / cancel /
/// unknown ids resolve to the dismissed empty map.
pub(crate) fn permission_response_to_answers(question: &Value, response: &Value) -> Value {
    let outcome = &response["outcome"];
    if outcome.get("outcome").and_then(Value::as_str) != Some("selected") {
        return json!({"answers": {}});
    }
    let option_id = outcome
        .get("optionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let question_text = question
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(index) = option_id.strip_prefix("q0_opt_")
        && let Ok(index) = index.parse::<usize>()
        && let Some(label) = question
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| options.get(index))
            .and_then(|option| option.get("label"))
            .and_then(Value::as_str)
    {
        return json!({"answers": {question_text: label}});
    }
    json!({"answers": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> Vec<Value> {
        vec![
            json!({
                "question": "Which language?",
                "header": "lang",
                "body": "pick one",
                "options": [
                    {"label": "Rust", "description": "fast"},
                    {"label": "TypeScript"},
                ],
            }),
            json!({
                "question": "Which features?",
                "multiSelect": true,
                "options": [{"label": "a"}, {"label": "b"}, {"label": "c"}],
            }),
        ]
    }

    // --- elicitation ------------------------------------------------------

    /// Given a client that advertised `elicitation.form`, when the ask is
    /// mapped, then the `elicitation/create` form-mode request carries
    /// every question natively — single-select as `string` + `oneOf`,
    /// multi-select as `array` + `items.anyOf` with `minItems: 1`.
    #[test]
    fn should_map_the_question_set_into_a_form_mode_elicitation_request() {
        let params = question_to_elicitation_params("session-1", &questions());
        assert_eq!(params["sessionId"], "session-1");
        assert_eq!(params["mode"], "form");
        assert_eq!(params["message"], "Which language?\nWhich features?");
        assert_eq!(params["requestedSchema"]["required"], json!(["q0", "q1"]));
        assert_eq!(
            params["requestedSchema"]["properties"]["q0"],
            json!({
                "type": "string",
                "title": "lang",
                "description": "pick one",
                "oneOf": [
                    {"const": "Rust", "title": "Rust", "description": "fast"},
                    {"const": "TypeScript", "title": "TypeScript"},
                ],
            })
        );
        assert_eq!(
            params["requestedSchema"]["properties"]["q1"],
            json!({
                "type": "array",
                "title": "Which features?",
                "minItems": 1,
                "items": {"anyOf": [
                    {"const": "a", "title": "a"},
                    {"const": "b", "title": "b"},
                    {"const": "c", "title": "c"},
                ]},
            })
        );
    }

    /// Given the client's elicitation responses, when they are mapped back,
    /// then an accept with declared values answers the questions (a
    /// multi-select joining in declared order) and decline / cancel /
    /// off-menu values resolve to the dismissed empty map.
    #[test]
    fn should_map_elicitation_responses_back_into_answers() {
        let questions = questions();
        let accept = json!({
            "action": "accept",
            "content": {"q0": "Rust", "q1": ["c", "a"]},
        });
        assert_eq!(
            elicitation_response_to_answers(&questions, &accept),
            json!({"answers": {"Which language?": "Rust", "Which features?": "a, c"}})
        );

        for response in [
            json!({"action": "decline"}),
            json!({"action": "cancel"}),
            json!({"action": "accept"}),
            json!({"action": "accept", "content": null}),
            json!({"action": "accept", "content": {"q0": "off-menu"}}),
        ] {
            assert_eq!(
                elicitation_response_to_answers(&questions, &response),
                json!({"answers": {}}),
                "a dismissed / unusable reply never fabricates an answer: {response}"
            );
        }
    }

    // --- degraded permission surface ---------------------------------------

    /// Given a client without the elicitation capability, when the ask is
    /// mapped, then only the first question is asked as a single-select
    /// permission surface with the `q0_opt_*` id namespace and a trailing
    /// Skip.
    #[test]
    fn should_degrade_to_a_single_select_permission_surface() {
        let params = question_to_permission_params("session-1", &questions());
        assert_eq!(params["sessionId"], "session-1");
        assert_eq!(params["toolCall"]["title"], "AskUserQuestion");
        assert_eq!(
            params["toolCall"]["content"],
            json!([{
                "type": "content",
                "content": {"type": "text", "text": "Which language?"},
            }])
        );
        assert_eq!(
            params["options"],
            json!([
                {"optionId": "q0_opt_0", "name": "Rust", "kind": "allow_once"},
                {"optionId": "q0_opt_1", "name": "TypeScript", "kind": "allow_once"},
                {"optionId": "q0_skip", "name": "Skip", "kind": "reject_once"},
            ])
        );
    }

    /// Given the degraded surface's responses, when they are mapped back,
    /// then a `q0_opt_<i>` selection answers with that label and skip /
    /// cancel / unknown ids resolve to the dismissed empty map.
    #[test]
    fn should_map_permission_responses_back_into_answers() {
        let question = questions().remove(0);
        assert_eq!(
            permission_response_to_answers(
                &question,
                &json!({"outcome": {"outcome": "selected", "optionId": "q0_opt_1"}}),
            ),
            json!({"answers": {"Which language?": "TypeScript"}})
        );
        for response in [
            json!({"outcome": {"outcome": "cancelled"}}),
            json!({"outcome": {"outcome": "selected", "optionId": "q0_skip"}}),
            json!({"outcome": {"outcome": "selected", "optionId": "q0_opt_99"}}),
            json!({"outcome": {"outcome": "selected", "optionId": "approve_once"}}),
            json!({}),
        ] {
            assert_eq!(
                permission_response_to_answers(&question, &response),
                json!({"answers": {}}),
                "an unusable reply never fabricates an answer: {response}"
            );
        }
    }

    // --- capability detection ----------------------------------------------

    /// Given the client capabilities advertised at initialize, when the
    /// elicitation surface is probed, then only a non-null `elicitation.
    /// form` counts as support.
    #[test]
    fn should_detect_the_elicitation_form_capability() {
        assert!(elicitation_form_supported(Some(&json!({
            "elicitation": {"form": {}},
        }))));
        assert!(!elicitation_form_supported(Some(&json!({
            "elicitation": {"form": null},
        }))));
        assert!(!elicitation_form_supported(Some(&json!({"fs": {}}))));
        assert!(!elicitation_form_supported(None));
    }
}
