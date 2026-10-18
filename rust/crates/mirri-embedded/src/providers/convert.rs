//! Shared stream plumbing for the provider adapters.
//!
//! [`spawn_provider_chat`] is the bridge between the two chat APIs:
//! [`ChatProvider::chat`](mirri_kosong::ChatProvider::chat) is an `async fn`
//! returning a concrete [`SseStream`], while
//! [`Llm::chat`](mirri_core::Llm::chat) is a sync `fn` returning a boxed
//! [`LlmEventStream`]. The async HTTP call is driven on a spawned task that
//! pumps normalized [`StreamEvent`]s through an mpsc channel into the stream
//! the caller consumes.
//!
//! This module owns the `SseStream` → `LlmEvent` translation; the per-provider
//! modules own the request-JSON translation.

use std::sync::Arc;

use mirri_core::cancel::CancelToken;
use mirri_core::llm::{LlmEvent, LlmEventStream};
use mirri_core::message::ContentPart;
use mirri_kosong::provider::{ChatProvider, StreamEvent};
use mirri_kosong::streaming::SseStream;
use serde_json::Value;
use tokio::sync::mpsc;

/// Capacity of the mpsc channel between the pump task and the returned
/// [`LlmEventStream`]. Backpressure is provider-side: when the consumer stops
/// draining, the pump stalls on `send` and the HTTP body stops being read.
const STREAM_CHANNEL_CAPACITY: usize = 128;

/// Drive `provider.chat(request)` on a spawned task and return the resulting
/// [`LlmEventStream`].
///
/// A non-`2xx` status or transport failure surfaces as a single
/// [`LlmEvent::Error`] (the provider converts the response before any
/// streaming begins); a successful response is pumped event-by-event by
/// [`pump_sse_to_llm_events`].
///
/// `ChatProvider` is not object-safe (its `chat` returns `impl Future`), so
/// this is generic over the concrete provider type `P`.
pub(crate) fn spawn_provider_chat<P>(
    provider: Arc<P>,
    request: Value,
    cancel: CancelToken,
) -> LlmEventStream
where
    P: ChatProvider + 'static,
{
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        match provider.chat(request).await {
            Ok(sse) => pump_sse_to_llm_events(sse, cancel, tx).await,
            Err(err) => {
                let _ = tx
                    .send(LlmEvent::Error {
                        message: err.to_string(),
                    })
                    .await;
            }
        }
    });
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}

/// Pump normalized provider [`StreamEvent`]s into the consumer channel as
/// [`LlmEvent`]s, stopping when the stream ends (EOF), the token is cancelled,
/// or a terminal event (`Done`/`Error`) has been seen and the provider closes
/// the stream.
///
/// Text and thinking deltas are forwarded incrementally *and* accumulated;
/// the accumulated block-level parts are flushed before a `ToolCall` and
/// before `Done`, so the consumer sees deltas for live rendering followed by
/// ordered block-level parts.
///
/// After a terminal event the pump keeps reading to EOF instead of returning
/// immediately: OpenAI (with `stream_options.include_usage`) sends the usage
/// chunk in a separate trailing SSE event *after* the `finish_reason` chunk,
/// and the engine's stream consumer drains to `None` rather than stopping at
/// `Done`, so abandoning the read at `Done` would drop OpenAI's usage
/// accounting. Duplicate terminal events are suppressed.
async fn pump_sse_to_llm_events(
    mut sse: SseStream,
    cancel: CancelToken,
    tx: mpsc::Sender<LlmEvent>,
) {
    let mut text_buffer = String::new();
    let mut think_buffer = String::new();
    let mut terminated = false;

    while let Some(result) = sse.next_event().await {
        if cancel.is_cancelled() {
            // Cancelled mid-stream (方式 B 取消闭合): flush the accumulated
            // partial text/thinking as block-level parts before breaking, so
            // the engine's stream consumer sees exactly the prefix that was
            // delivered to the user and can close the turn append-only. The
            // stream ends without a `Done` event, which the consumer reads
            // as "interrupted".
            flush_text_parts(&mut text_buffer, &mut think_buffer, &tx).await;
            break;
        }
        match result {
            Ok(StreamEvent::MessageStart { .. }) => { /* informational; skipped */ }
            Ok(StreamEvent::TextDelta { text }) => {
                text_buffer.push_str(&text);
                if tx.send(LlmEvent::TextDelta { text }).await.is_err() {
                    break;
                }
            }
            Ok(StreamEvent::ThinkDelta { text }) => {
                think_buffer.push_str(&text);
                if tx.send(LlmEvent::ThinkDelta { text }).await.is_err() {
                    break;
                }
            }
            Ok(StreamEvent::ToolCallPart {
                index,
                arguments_part,
                call_id,
                tool_name,
            }) => {
                if tx
                    .send(LlmEvent::ToolCallPart {
                        index,
                        arguments_part,
                        call_id,
                        tool_name,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            }) => {
                flush_text_parts(&mut text_buffer, &mut think_buffer, &tx).await;
                if tx
                    .send(LlmEvent::ToolCall {
                        tool,
                        args,
                        call_id,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            }) => {
                // May arrive before Done (Anthropic `message_start` /
                // `message_delta`) or after Done (OpenAI trailing usage
                // chunk) — always forward it.
                if tx
                    .send(LlmEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(StreamEvent::Done { stop_reason }) => {
                if terminated {
                    continue;
                }
                flush_text_parts(&mut text_buffer, &mut think_buffer, &tx).await;
                if tx.send(LlmEvent::Done { stop_reason }).await.is_err() {
                    break;
                }
                terminated = true;
            }
            Ok(StreamEvent::Error { message }) => {
                if terminated {
                    continue;
                }
                if tx.send(LlmEvent::Error { message }).await.is_err() {
                    break;
                }
                terminated = true;
            }
            Err(err) => {
                if tx
                    .send(LlmEvent::Error {
                        message: err.to_string(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                break;
            }
        }
    }
}

/// Forward the accumulated text and thinking parts, in that order, clearing
/// the buffers. The caller is responsible for ordering: the block-level
/// transcript interleaves `TextPart`/`ThinkPart`/`ToolCall` in emission order,
/// matching the provider's content-block order.
async fn flush_text_parts(
    text_buffer: &mut String,
    think_buffer: &mut String,
    tx: &mpsc::Sender<LlmEvent>,
) {
    if !text_buffer.is_empty()
        && tx
            .send(LlmEvent::TextPart {
                text: std::mem::take(text_buffer),
            })
            .await
            .is_err()
    {
        return;
    }
    if !think_buffer.is_empty()
        && tx
            .send(LlmEvent::ThinkPart {
                text: std::mem::take(think_buffer),
            })
            .await
            .is_err()
    {
        // Consumer dropped — nothing more to flush; the caller's next send
        // will detect the error and break the pump loop.
    }
}

// ---------------------------------------------------------------------------
// File-attachment inlining
// ---------------------------------------------------------------------------

/// Make user-message file attachments model-visible: the chat-completions
/// family of wire formats has no file part, so a `File` content part would
/// otherwise be silently dropped by every adapter's conversion.
///
/// Text attachments (`text/*` and the textual `application/*` types) are
/// inlined as an enveloped text part carrying the decoded content; binary
/// attachments become a descriptor placeholder naming what was attached (the
/// model can still acknowledge it and read it through file tools).
pub(crate) fn inline_file_attachments(parts: &[ContentPart]) -> Vec<ContentPart> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::File {
                data,
                name,
                media_type,
                size,
                ..
            } => ContentPart::Text {
                text: file_attachment_text(
                    data.as_deref(),
                    name.as_deref(),
                    media_type.as_deref(),
                    *size,
                ),
            },
            other => other.clone(),
        })
        .collect()
}

/// Render the model-visible text for one file attachment.
fn file_attachment_text(
    data: Option<&str>,
    name: Option<&str>,
    media_type: Option<&str>,
    size: Option<u64>,
) -> String {
    let name_attr = name
        .map(|name| format!(" name=\"{name}\""))
        .unwrap_or_default();
    let textual = media_type.is_none_or(is_textual_media_type);
    match data.filter(|_| textual).and_then(decode_data_url) {
        Some(decoded) => {
            let media_attr = media_type
                .map(|media| format!(" media-type=\"{media}\""))
                .unwrap_or_default();
            format!("<attachment{name_attr}{media_attr}>\n{decoded}\n</attachment>")
        }
        None if data.is_some() => format!(
            "[attachment{name_attr}: binary content ({}, {} bytes) not inlined]",
            media_type.unwrap_or("unknown media type"),
            size.unwrap_or(0)
        ),
        None => format!("[attachment{name_attr}: content unavailable]"),
    }
}

/// Whether a media type carries textual content the model can read inline.
fn is_textual_media_type(media_type: &str) -> bool {
    let media = media_type.trim().to_ascii_lowercase();
    media.starts_with("text/")
        || media.ends_with("+json")
        || media.ends_with("+xml")
        || matches!(
            media.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/javascript"
                | "application/typescript"
                | "application/x-sh"
                | "application/sql"
        )
}

/// Decode a `data:<mime>;base64,<payload>` URL to its UTF-8 content.
fn decode_data_url(data_url: &str) -> Option<String> {
    let (_, payload) = data_url.split_once(',')?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload).ok()?;
    String::from_utf8(bytes).ok()
}
