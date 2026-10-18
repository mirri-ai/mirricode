//! Model-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B5-b klient
//! `modelService` record methods (`model.get` / `model.set` /
//! `model.delete`) and the `modelResolver` catalog methods
//! (`model.getProvider` / `model.setDefaultModel`).
//!
//! Storage landing (recorded in the TSV rows and the task report): the
//! records live in the config.toml `[models.<id>]` / `[providers.<id>]`
//! sections — the same sections the TS engine's kosong registries persist
//! through — reached via the raw TOML document so klient passthrough keys
//! (`providerId`, `baseUrl`, `overrides`, ...) survive a write/read cycle
//! even though the typed [`ModelAlias`] does not model them. Writes ride
//! the same load-without-env → mutate → persist → reload pipeline the
//! Settled `config.set` uses.
//!
//! DRIFT (honest, recorded in the TSV rows): the rust `models` section is
//! a typed alias map, so a record missing the alias trio
//! (`provider` / `model` / `maxContextSize`) is rejected with invalid
//! params — the TS `ModelRecordSchema` allows flat baseUrl-only records.
//! Record outputs are redacted with the `apiKey` → `[REDACTED]` rule the
//! Settled `config.get` wire applies. Unknown-id reads answer JSON `null`
//! (the klient `maybe` semantics); unknown-id catalog lookups answer the
//! TS `provider.not_found` / `model.not_found` domain errors.

use std::sync::Arc;

use futures::StreamExt;
use mirri_transport::adapter::EngineError;
use mirri_transport::frame::JsonRpcNotification;
use serde_json::{Value, json};

use super::config_handlers::{first_string_arg, redact_api_keys};
use super::responses::{ModelGenerateResult, ModelGenerateUsage, to_result};
use super::{EmbeddedServer, engine_error, invalid_params, not_found_domain};
use crate::providers::config_factory::resolve_provider_api_key;
use crate::providers::{AnthropicLlm, GoogleLlm, KimiLlm, OpenAILlm, OpenAIResponsesLlm};
use mirri_core::EngineError as CoreEngineError;
use mirri_core::llm::{LlmChatParams, LlmEvent};
use mirri_core::message::Message;
use mirri_core::tool::ToolDefinition;
use mirri_core::{CancelToken, Llm, ThinkingEffort, resolve_model_alias_in};

impl EmbeddedServer {
    /// `model.get(id)` — the modelService record for one alias, or JSON
    /// `null` for an unknown id (the klient `maybe` semantics).
    pub(super) async fn dispatch_model_get(&self, params: Value) -> Result<Value, EngineError> {
        let id = first_string_arg(&params, &["id", "modelId"])?;
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let records = config.to_model_records_wire();
        Ok(records
            .get(&id)
            .map(|record| redact_api_keys(record.clone()))
            .unwrap_or(Value::Null))
    }

    /// `model.set(id, config)` — replace the modelService record for one
    /// alias (a full-record replace, matching the TS service), then persist
    /// and reload. noResult on the wire.
    pub(super) async fn dispatch_model_set(&self, params: Value) -> Result<Value, EngineError> {
        let (id, record) = record_args(&params, &["id", "modelId"], &["config", "model"])?;
        self.set_record_section("models", &id, record)?;
        Ok(Value::Null)
    }

    /// `model.delete(id)` — remove the modelService record (and clear the
    /// global `defaultModel` pointer when it addressed the deleted alias,
    /// so it never dangles). noResult on the wire.
    pub(super) async fn dispatch_model_delete(&self, params: Value) -> Result<Value, EngineError> {
        let id = first_string_arg(&params, &["id", "modelId"])?;
        self.mutate_config_raw(|config| {
            config
                .set_raw_section_entry("models", &id, None)
                .map_err(engine_error)?;
            if config.default_model.as_deref() == Some(id.as_str()) {
                config
                    .replace_raw_sections(&[("default_model".to_string(), None)])
                    .map_err(engine_error)?;
            }
            Ok(())
        })?;
        Ok(Value::Null)
    }

    /// `model.getProvider(id)` — the modelResolver provider catalog item
    /// for one configured provider, or the TS `provider.not_found` domain
    /// error for an unknown id.
    pub(super) async fn dispatch_model_get_provider(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let provider_id = first_string_arg(&params, &["providerId", "id", "name"])?;
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let providers = config.to_provider_records_wire();
        let Some(provider) = providers.get(&provider_id) else {
            return Err(not_found_domain(
                "provider.not_found",
                format!("provider {provider_id} does not exist"),
            ));
        };
        let models = config.to_model_records_wire();
        Ok(provider_catalog_item(
            &provider_id,
            provider,
            &models,
            config.default_model.as_deref(),
        ))
    }

    /// `model.setDefaultModel(id)` — point the global `defaultModel` at a
    /// configured alias and answer the klient
    /// `setDefaultModelResponseSchema` shape; an unknown alias answers the
    /// TS `model.not_found` domain error. The id may be a `provider/model`
    /// composite (the C6 id the web pickers compose) — it resolves to the
    /// configured alias naming the same provider + model pair, and the
    /// pointer is stored under that canonical alias key so every later
    /// exact-key read addresses a real alias.
    pub(super) async fn dispatch_model_set_default_model(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let model_id = first_string_arg(&params, &["modelId", "id"])?;
        let resolved_id = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            match resolve_model_alias_in(&config.to_model_records_wire(), &model_id) {
                Some(key) => key,
                // The already-set pointer may be re-set idempotently even
                // when it addresses no alias (the pre-composite behavior).
                None if config.default_model.as_deref() == Some(model_id.as_str()) => {
                    model_id.clone()
                }
                None => {
                    return Err(not_found_domain(
                        "model.not_found",
                        format!("model {model_id} does not exist"),
                    ));
                }
            }
        };
        self.mutate_config_raw(|config| {
            config
                .replace_raw_sections(&[(
                    "default_model".to_string(),
                    Some(Value::String(resolved_id.clone())),
                )])
                .map_err(engine_error)
        })?;
        // Answer from the reloaded snapshot: the alias's catalog item plus
        // the new default pointer (the klient setDefaultModelResponse shape).
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let models = config.to_model_records_wire();
        let providers = config.to_provider_records_wire();
        let alias = models
            .get(&resolved_id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        Ok(json!({
            "default_model": resolved_id,
            "model": model_catalog_item(&alias, &providers),
        }))
    }

    /// `model.generate(modelId, input, params?)` — the klient
    /// `modelResolver.generate` deep method: a single LLM call with no
    /// session, no turn mechanism, no transcript, and no tool execution.
    ///
    /// A per-request provider adapter is constructed from the config's
    /// `[models.<id>]` → `[providers.<id>]` chain (design notes §1), the
    /// `LlmEvent` stream is mapped onto the klient chunk vocabulary
    /// (`part` / `usage` / `finish`), and each chunk is broadcast as a
    /// best-effort `event.generate.chunk` notification carrying the
    /// requestId. The **aggregated response is the authoritative face**
    /// (requestId + finishReason + usage + the assembled assistant
    /// message); chunk delivery is fire-and-forget (try_send).
    ///
    /// DRIFT (recorded in the TSV row): `timing` chunks are never emitted
    /// (the TS side measures client-side latencies the rust engine cannot
    /// observe); `cacheKey`/`temperature`/`topP`/`maxCompletionTokens` and
    /// `responseFormat` are accepted but not applied (no per-request
    /// sampling plumbing exists on the `Llm` adapters); the `usage` chunk
    /// carries the two-field rust accounting, not the four-field TS
    /// `TokenUsage`.
    pub(super) async fn dispatch_model_generate(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (model_id, input, generate_params) = generate_args(&params)?;
        validate_generate_input(&input)?;
        let request_id = generate_params
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Resolve the alias → provider chain from the config snapshot. The
        // id may be a `provider/model` composite (the C6 id the web pickers
        // compose) — it resolves to the configured alias naming the same
        // provider + model pair.
        let llm = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            let models = config.to_model_records_wire();
            let Some(alias) = resolve_model_alias_in(&models, &model_id)
                .and_then(|key| models.get(key.as_str()).cloned())
            else {
                return Err(not_found_domain(
                    "model.not_found",
                    format!("model {model_id} does not exist"),
                ));
            };
            let Some(provider_id) = alias
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return Err(invalid_params(format!(
                    "model {model_id} has no provider configured"
                )));
            };
            let providers = config.to_provider_records_wire();
            let Some(provider) = providers.get(&provider_id) else {
                return Err(not_found_domain(
                    "provider.not_found",
                    format!("provider {provider_id} does not exist"),
                ));
            };
            let provider_type = provider
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("openai")
                .to_string();
            // Resolve the wire protocol before anything else — the
            // alias-level `protocol` overrides the provider type, and the
            // provider type only works when it is itself a wire protocol
            // or a registered vendor (the TS `resolveProtocol` chain).
            let Some(wire_protocol) = resolve_wire_protocol(
                alias.get("protocol").and_then(Value::as_str),
                provider_type.as_str(),
            ) else {
                return Err(invalid_params(format!(
                    "model {model_id} must declare a wire protocol (config: models.<id>.protocol)"
                )));
            };
            let is_kimi_vendor = provider_type.eq_ignore_ascii_case("kimi");
            // API key: the same field > env sub-table > shell chain the
            // agent-turn `ConfigAwareLlmFactory` resolves (a provider whose
            // credentials live only in `[providers.<id>.env]` or the shell
            // must not silently send an unauthenticated request here).
            let api_key = resolve_provider_api_key(provider, &provider_type).unwrap_or_default();
            let base_url = provider
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match (wire_protocol, is_kimi_vendor) {
                    ("anthropic", _) => "https://api.anthropic.com/v1".to_string(),
                    ("google-genai", _) => {
                        "https://generativelanguage.googleapis.com/v1beta".to_string()
                    }
                    (_, true) => "https://api.mirricode.com/coding/v1".to_string(),
                    _ => "https://api.openai.com/v1".to_string(),
                });
            let model_name = alias
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&model_id)
                .to_string();
            let system_prompt = input
                .get("systemPrompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let thinking_effort = generate_params
                .get("thinkingEffort")
                .and_then(Value::as_str)
                .map(|effort| {
                    serde_json::from_value::<ThinkingEffort>(Value::String(effort.to_string()))
                })
                .transpose()
                .map_err(|e| invalid_params(format!("invalid thinkingEffort: {e}")))?;

            let llm: std::sync::Arc<dyn Llm> = match wire_protocol {
                "anthropic" => Arc::new(AnthropicLlm::new(
                    api_key,
                    base_url,
                    model_name,
                    system_prompt,
                    thinking_effort,
                )),
                "openai" if is_kimi_vendor => Arc::new(KimiLlm::new(
                    api_key,
                    base_url,
                    model_name,
                    system_prompt,
                    thinking_effort,
                )),
                "openai" => Arc::new(OpenAILlm::new(
                    api_key,
                    base_url,
                    model_name,
                    system_prompt,
                    thinking_effort,
                )),
                "openai_responses" => Arc::new(OpenAIResponsesLlm::new(
                    api_key,
                    base_url,
                    model_name,
                    system_prompt,
                    thinking_effort,
                )),
                // The google wire protocol over the GoogleLlm adapter —
                // the same adapter `ProviderLlmFactory` serves for the
                // env-driven path.
                _ => Arc::new(GoogleLlm::new(
                    api_key,
                    base_url,
                    model_name,
                    system_prompt,
                    thinking_effort,
                )),
            };
            llm
        };

        // Translate the klient messages/tools onto the wire shapes the
        // adapters consume.
        let messages = klient_messages(&input)?;
        let tools = klient_tools(&input)?;

        let mut stream = llm
            .chat(LlmChatParams {
                messages,
                tools,
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            })
            .map_err(|e| engine_error(CoreEngineError::ProviderError(e.to_string())))?;

        // Pump the stream: every delta-level event is forwarded as a
        // `part` chunk (best-effort) and accumulated; `Usage` is forwarded
        // as-is; `Done` records the finish reason. Block-level
        // `TextPart`/`ThinkPart` re-emissions are skipped (their deltas were
        // already forwarded). A stream-level `Error` fails the whole call.
        let mut text_buffer = String::new();
        let mut think_buffer = String::new();
        let mut tool_calls: Vec<(String, String, Value)> = Vec::new();
        let mut usage: Option<(u64, u64, u64, u64)> = None;
        let mut finish_reason: Option<String> = None;
        let mut stream_error: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                LlmEvent::TextDelta { text } => {
                    text_buffer.push_str(&text);
                    self.emit_generate_chunk(
                        &request_id,
                        &model_id,
                        json!({
                            "type": "part",
                            "part": { "type": "text", "text": text },
                        }),
                    );
                }
                LlmEvent::ThinkDelta { text } => {
                    think_buffer.push_str(&text);
                    self.emit_generate_chunk(
                        &request_id,
                        &model_id,
                        json!({
                            "type": "part",
                            "part": { "type": "think", "think": text },
                        }),
                    );
                }
                LlmEvent::ToolCallPart {
                    index,
                    arguments_part,
                    ..
                } => {
                    self.emit_generate_chunk(
                        &request_id,
                        &model_id,
                        json!({
                            "type": "part",
                            "part": {
                                "type": "tool_call_part",
                                "argumentsPart": arguments_part,
                                "index": index,
                            },
                        }),
                    );
                }
                LlmEvent::ToolCall {
                    tool,
                    args,
                    call_id,
                } => {
                    tool_calls.push((call_id.clone(), tool.clone(), args.clone()));
                    self.emit_generate_chunk(
                        &request_id,
                        &model_id,
                        json!({
                            "type": "part",
                            "part": {
                                "type": "function",
                                "id": call_id,
                                "name": tool,
                                "arguments": args.to_string(),
                            },
                        }),
                    );
                }
                LlmEvent::TextPart { .. } | LlmEvent::ThinkPart { .. } => {}
                LlmEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                } => {
                    usage = Some((
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    ));
                    self.emit_generate_chunk(
                        &request_id,
                        &model_id,
                        json!({
                            "type": "usage",
                            "usage": {
                                "inputTokens": input_tokens,
                                "outputTokens": output_tokens,
                                "cacheReadTokens": cache_read_tokens,
                                "cacheCreationTokens": cache_creation_tokens,
                            },
                        }),
                    );
                }
                LlmEvent::Done { stop_reason } => {
                    finish_reason = Some(stop_reason);
                }
                LlmEvent::Error { message } => {
                    stream_error = Some(message);
                    break;
                }
            }
        }
        if let Some(message) = stream_error {
            return Err(engine_error(CoreEngineError::ProviderError(message)));
        }
        let Some(finish_reason) = finish_reason else {
            return Err(engine_error(CoreEngineError::ProviderError(
                "the provider stream ended without a terminal event".to_string(),
            )));
        };

        // Assemble the aggregated assistant message in the klient Message
        // shape (camelCase; `think` parts use the TS `think` key).
        let mut content = Vec::new();
        if !text_buffer.is_empty() {
            content.push(json!({ "type": "text", "text": text_buffer }));
        }
        if !think_buffer.is_empty() {
            content.push(json!({ "type": "think", "think": think_buffer }));
        }
        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), json!("assistant"));
        message.insert("content".to_string(), Value::Array(content));
        if !tool_calls.is_empty() {
            message.insert(
                "toolCalls".to_string(),
                Value::Array(
                    tool_calls
                        .iter()
                        .map(|(id, name, args)| {
                            json!({
                                "type": "function",
                                "id": id,
                                "name": name,
                                "arguments": args.to_string(),
                            })
                        })
                        .collect(),
                ),
            );
        }
        let message = Value::Object(message);

        // The finish chunk closes the stream, then the authoritative
        // aggregated response answers the call.
        self.emit_generate_chunk(
            &request_id,
            &model_id,
            json!({
                "type": "finish",
                "message": message,
                "finishReason": finish_reason,
                "id": request_id,
            }),
        );
        let (input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) =
            usage.unwrap_or((0, 0, 0, 0));
        to_result(ModelGenerateResult {
            request_id,
            finish_reason,
            usage: ModelGenerateUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            },
            message,
        })
    }

    /// Broadcast one `event.generate.chunk` notification (params carrying
    /// the requestId and the model id) to every live event subscriber.
    ///
    /// Best-effort by design (the controller adjudication): a full channel
    /// drops the chunk rather than blocking the pump — the aggregated
    /// `model.generate` response is the authoritative face.
    fn emit_generate_chunk(&self, request_id: &str, model_id: &str, chunk: Value) {
        let notification = JsonRpcNotification::new(
            "event.generate.chunk",
            json!({ "requestId": request_id, "model": model_id, "chunk": chunk }),
        );
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for subscriber in subscribers {
            let _ = subscriber.try_send(notification.clone());
        }
    }

    /// Shared record writer for `model.set` / `provider.set`: validate the
    /// record is an object, convert its **top-level** keys to the raw
    /// document's snake_case spelling (matching the TS `modelsToToml` /
    /// `providerEntryToToml` transforms — nested value maps such as
    /// `customHeaders` / `env` keep their keys verbatim, and a model's
    /// `overrides` object converts recursively like the TS
    /// `modelOverridesToToml`), and run the standard mutation pipeline.
    pub(super) fn set_record_section(
        &self,
        section: &str,
        id: &str,
        record: Value,
    ) -> Result<(), EngineError> {
        let Some(map) = record.as_object().cloned() else {
            return Err(invalid_params(format!(
                "the {section} record for '{id}' must be an object"
            )));
        };
        let mut snake_record = serde_json::Map::new();
        for (key, mut value) in map {
            if section == "models" && key == "overrides" {
                mirri_core::convert_keys_to_snake_case(&mut value);
            }
            snake_record.insert(mirri_core::camel_to_snake(&key), value);
        }
        let snake_record = Value::Object(snake_record);
        self.mutate_config_raw(|config| {
            config
                .set_raw_section_entry(section, id, Some(&snake_record))
                .map_err(engine_error)
        })
    }
}

/// Extract an `(id, record)` pair, accepting the object spelling and the
/// positional tuple spelling (`[id, record]`).
pub(super) fn record_args(
    params: &Value,
    id_names: &[&str],
    record_names: &[&str],
) -> Result<(String, Value), EngineError> {
    let params = if params.is_null() {
        Value::Object(Default::default())
    } else {
        params.clone()
    };
    match params {
        Value::Array(items) => {
            if items.len() != 2 {
                return Err(invalid_params("expected [id, record]"));
            }
            let id = items[0]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_params("expected the first argument to be a string"))?;
            Ok((id, items[1].clone()))
        }
        Value::Object(map) => {
            let mut id = None;
            for name in id_names {
                if let Some(Value::String(s)) = map.get(*name) {
                    id = Some(s.clone());
                    break;
                }
            }
            let id = id.ok_or_else(|| {
                invalid_params(format!(
                    "missing required string argument (one of {id_names:?})"
                ))
            })?;
            let mut record = None;
            for name in record_names {
                if let Some(r) = map.get(*name) {
                    record = Some(r.clone());
                    break;
                }
            }
            let record =
                record.ok_or_else(|| invalid_params("missing required record argument"))?;
            Ok((id, record))
        }
        _ => Err(invalid_params("expected an object or positional tuple")),
    }
}

/// Extract the `model.generate` positional tuple —
/// `[modelId, {systemPrompt, messages, tools?, responseFormat?}, params?]` —
/// validating the klient-required fields up front (`systemPrompt` and
/// `messages`), keeping the optional third element's unknown keys
/// (including the rust-side `requestId` extension) verbatim.
fn generate_args(params: &Value) -> Result<(String, Value, Value), EngineError> {
    // The klient wire sends the positional tuple
    // `[modelId, input, params?]`; the object spelling
    // (`{modelId, systemPrompt, messages, ...}` with the generate options
    // under an optional `params` key) is also accepted, mirroring the
    // record_args tolerance of the other model methods.
    match params {
        Value::Array(items) => {
            if items.len() < 2 || items.len() > 3 {
                return Err(invalid_params(
                    "model.generate expects a positional [modelId, input, params?] tuple",
                ));
            }
            let model_id = items[0]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_params("modelId must be a string"))?;
            let Some(input) = items[1].as_object() else {
                return Err(invalid_params("the generate input must be an object"));
            };
            let generate_params = match items.get(2) {
                None | Some(Value::Null) => Value::Object(Default::default()),
                Some(value) => value
                    .as_object()
                    .map(|map| Value::Object(map.clone()))
                    .ok_or_else(|| invalid_params("the generate params must be an object"))?,
            };
            Ok((model_id, Value::Object(input.clone()), generate_params))
        }
        Value::Object(map) => {
            let model_id = ["modelId", "id"]
                .iter()
                .find_map(|key| map.get(*key).and_then(Value::as_str))
                .map(str::to_string)
                .ok_or_else(|| invalid_params("modelId must be a string"))?;
            let generate_params = match map.get("params").or_else(|| map.get("generateParams")) {
                None | Some(Value::Null) => Value::Object(Default::default()),
                Some(value) => value
                    .as_object()
                    .map(|object| Value::Object(object.clone()))
                    .ok_or_else(|| invalid_params("the generate params must be an object"))?,
            };
            Ok((model_id, Value::Object(map.clone()), generate_params))
        }
        _ => Err(invalid_params(
            "model.generate expects a positional [modelId, input, params?] tuple",
        )),
    }
}

/// Validate the klient-required generate-input fields (`systemPrompt`,
/// `messages`, and an array `tools` when present).
fn validate_generate_input(input: &Value) -> Result<(), EngineError> {
    let Some(object) = input.as_object() else {
        return Err(invalid_params("the generate input must be an object"));
    };
    if !object.contains_key("systemPrompt") {
        return Err(invalid_params("systemPrompt is required"));
    }
    if !input.get("messages").is_some_and(Value::is_array) {
        return Err(invalid_params("messages must be an array"));
    }
    if let Some(tools) = input.get("tools")
        && !tools.is_null()
        && !tools.is_array()
    {
        return Err(invalid_params("tools must be an array"));
    }
    Ok(())
}

/// Translate the klient `messages` array onto the wire
/// [`Message`](mirri_core::message::Message) shape, tolerating the two
/// spellings the TS side sends and the rust type does not model: a plain
/// string `content` (wrapped into one text part) and `think` parts keyed
/// `think` (the rust `ContentPart` serializes `text`).
fn klient_messages(input: &Value) -> Result<Vec<Message>, EngineError> {
    let messages = input
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_params("messages must be an array"))?;
    let mut translated = Vec::with_capacity(messages.len());
    for message in messages {
        let mut message = message.clone();
        if let Some(Value::String(text)) = message.get("content").cloned() {
            message["content"] = json!([{ "type": "text", "text": text }]);
        }
        if let Some(Value::Array(parts)) = message.get_mut("content") {
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) == Some("think")
                    && let Some(think) = part.get("think").cloned()
                {
                    part.as_object_mut().unwrap().remove("think");
                    part.as_object_mut()
                        .unwrap()
                        .insert("text".to_string(), think);
                }
            }
        }
        let message: Message = serde_json::from_value(message)
            .map_err(|e| invalid_params(format!("invalid message in messages: {e}")))?;
        translated.push(message);
    }
    Ok(translated)
}

/// Translate the klient `tools` array (the kosong `Tool` shape:
/// `{name, description?, parameters?}`) onto
/// [`ToolDefinition`](mirri_core::tool::ToolDefinition)s. Execution never
/// happens — the definitions only describe what the model may call.
fn klient_tools(input: &Value) -> Result<Vec<ToolDefinition>, EngineError> {
    let Some(tools) = input.get("tools").filter(|tools| !tools.is_null()) else {
        return Ok(Vec::new());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_params("tools must be an array"))?;
    let mut translated = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| invalid_params("every tool needs a name"))?;
        translated.push(ToolDefinition {
            name,
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            parameters: tool.get("parameters").cloned().unwrap_or(json!({})),
            deferred: false,
        });
    }
    Ok(translated)
}

/// Project one configured provider as the klient `providerCatalogItem`
/// wire shape (snake_case; the `modelResolver.listProviders` element
/// contract).
pub(super) fn provider_catalog_item(
    provider_id: &str,
    provider: &Value,
    models: &Value,
    _global_default_model: Option<&str>,
) -> Value {
    let has_api_key = provider
        .get("apiKey")
        .and_then(Value::as_str)
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    let provider_models: Vec<String> = models
        .as_object()
        .map(|models| {
            models
                .iter()
                .filter(|(_, record)| {
                    record.get("provider").and_then(Value::as_str) == Some(provider_id)
                })
                .map(|(_, record)| {
                    record
                        .get("model")
                        .or_else(|| record.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();

    let mut item = serde_json::Map::new();
    item.insert("id".to_string(), json!(provider_id));
    item.insert(
        "type".to_string(),
        json!(
            provider
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("openai")
        ),
    );
    item.insert("has_api_key".to_string(), json!(has_api_key));
    item.insert(
        "status".to_string(),
        json!(if has_api_key {
            "connected"
        } else {
            "unconfigured"
        }),
    );
    item.insert("models".to_string(), json!(provider_models));
    if let Some(bu) = provider.get("baseUrl").filter(|v| !v.is_null()) {
        item.insert("base_url".to_string(), bu.clone());
    }
    if let Some(dm) = provider.get("defaultModel").filter(|v| !v.is_null()) {
        item.insert("default_model".to_string(), dm.clone());
    }
    Value::Object(item)
}

/// Project one configured model record as the klient `modelCatalogItem`
/// wire shape (snake_case; the `modelResolver.listModels` element
/// contract, the same projection `model.list` serves). The config key
/// (alias id) stays off the wire — the klient schema has no `id` field
/// and the model-settings adjudication (T3) keeps it there; UI ids are
/// composed client-side as `provider/model`.
pub(super) fn model_catalog_item(alias: &Value, providers: &Value) -> Value {
    let provider_id = alias.get("provider").and_then(Value::as_str).unwrap_or("");
    let _ = providers
        .get(provider_id)
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("openai");

    let mut item = serde_json::Map::new();
    item.insert("provider".to_string(), json!(provider_id));
    item.insert(
        "model".to_string(),
        alias
            .get("model")
            .or_else(|| alias.get("name"))
            .cloned()
            .unwrap_or(json!("")),
    );
    item.insert(
        "max_context_size".to_string(),
        alias.get("maxContextSize").cloned().unwrap_or(json!(0)),
    );
    if let Some(dn) = alias.get("displayName").filter(|v| !v.is_null()) {
        item.insert("display_name".to_string(), dn.clone());
    }
    if let Some(desc) = alias.get("description").filter(|v| !v.is_null()) {
        item.insert("description".to_string(), desc.clone());
    }
    if let Some(caps) = alias.get("capabilities").filter(|v| !v.is_null()) {
        item.insert("capabilities".to_string(), caps.clone());
    }
    if let Some(se) = alias.get("supportEfforts").filter(|v| !v.is_null()) {
        item.insert("support_efforts".to_string(), se.clone());
    }
    if let Some(de) = alias.get("defaultEffort").filter(|v| !v.is_null()) {
        item.insert("default_effort".to_string(), de.clone());
    }
    Value::Object(item)
}

/// Resolve the wire protocol for a model alias, mirroring the TypeScript
/// `resolveProtocol` chain (`catalogService.ts`): the alias-level
/// `models.<id>.protocol` wins; otherwise the provider type when it is
/// itself a wire protocol or a registered vendor (`kimi` over the OpenAI
/// wire); otherwise no protocol exists and the request is invalid. Shared
/// with the session handlers, whose usage capability bit follows the same
/// resolution.
pub(super) fn resolve_wire_protocol(
    model_protocol: Option<&str>,
    provider_type: &str,
) -> Option<&'static str> {
    if let Some(protocol) = model_protocol.and_then(normalize_wire_protocol) {
        return Some(protocol);
    }
    if let Some(protocol) = normalize_wire_protocol(provider_type) {
        return Some(protocol);
    }
    if provider_type.eq_ignore_ascii_case("kimi") {
        return Some("openai");
    }
    None
}

/// Map a config spelling onto the canonical wire-protocol id, accepting
/// the tolerated aliases (`openai_responses` / `openai-responses` /
/// `openairesponses`, `google-genai` / `google_genai` / `google`).
fn normalize_wire_protocol(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Some("anthropic"),
        "openai" => Some("openai"),
        "openai_responses" | "openai-responses" | "openairesponses" => Some("openai_responses"),
        "google-genai" | "google_genai" | "google" => Some("google-genai"),
        _ => None,
    }
}
