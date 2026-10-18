//! `message_pipeline` — pluggable post-projection step chain.
//!
//! Ports the TypeScript `messagePipeline` domain. After the context memory
//! projects the stored history into a wire-valid message array, a chain of
//! [`ProjectionStep`]s can further transform that array before it is sent to
//! the provider — for example, resolving media references the target model
//! cannot handle.
//!
//! Steps are registered on a [`MessagePipeline`] and run, in registration
//! order, only for projections they declare applicable via
//! [`ProjectionStep::applies_to`]. Each step receives the output of the
//! previous step.

use crate::llm::ModelCapability;
use crate::media_placeholder::{MediaKind, PlaceholderCause, build_media_placeholder};
use crate::message::{ContentPart, Message};

/// A single transformation applied to a projected message array.
///
/// Steps must be cheap, deterministic, and side-effect free: they receive an
/// owned vector and return a new one.
pub trait ProjectionStep: Send + Sync {
    /// Stable identifier for this step (used for diagnostics and ordering).
    fn name(&self) -> &str;

    /// Whether this step should run for the given projection name
    /// (e.g. `"normal"`, `"strict"`, `"degraded"`).
    fn applies_to(&self, projection: &str) -> bool;

    /// Transform the messages, returning the new array.
    fn process(&self, messages: Vec<Message>) -> Vec<Message>;
}

/// Ordered chain of [`ProjectionStep`]s applied after projection.
pub struct MessagePipeline {
    steps: Vec<Box<dyn ProjectionStep + Send + Sync>>,
}

impl Default for MessagePipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl MessagePipeline {
    /// Create an empty pipeline.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Append a step to the end of the chain.
    pub fn register(&mut self, step: Box<dyn ProjectionStep + Send + Sync>) {
        self.steps.push(step);
    }

    /// Run every applicable step, in registration order, over the messages.
    ///
    /// Each step receives the output of the previous step; non-applicable
    /// steps are skipped entirely.
    pub fn run(&self, projection: &str, messages: Vec<Message>) -> Vec<Message> {
        let mut current = messages;
        for step in &self.steps {
            if step.applies_to(projection) {
                current = step.process(current);
            }
        }
        current
    }

    /// The number of registered steps.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// The names of the registered steps, in registration order.
    pub fn step_names(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|step| step.name().to_string())
            .collect()
    }
}

/// Resolves `video_url` content parts into text placeholders for models that
/// do not ingest video.
///
/// When `supports_video` is `false`, every [`ContentPart::VideoUrl`] is
/// replaced with a [`ContentPart::Text`] placeholder naming the URL, so the
/// model at least sees a reference to the omitted media.
///
/// ADR-003 revision: an undeclared capability is treated as unsupported —
/// the step applies to every model with `video_in == false`.
pub struct VideoReferenceResolver {
    supports_video: bool,
}

impl VideoReferenceResolver {
    /// Create a resolver bound to the model's capability state.
    ///
    /// Inert when video input is supported.
    pub fn for_capability(caps: &ModelCapability) -> Self {
        Self {
            supports_video: caps.video_in,
        }
    }

    /// Create a resolver for an explicit video capability. When
    /// `supports_video` is `true` the step is inert.
    pub fn new(supports_video: bool) -> Self {
        Self { supports_video }
    }
}

impl ProjectionStep for VideoReferenceResolver {
    fn name(&self) -> &str {
        "video_reference_resolver"
    }

    fn applies_to(&self, _projection: &str) -> bool {
        !self.supports_video
    }

    fn process(&self, messages: Vec<Message>) -> Vec<Message> {
        if self.supports_video {
            return messages;
        }
        messages
            .into_iter()
            .map(|mut message| {
                message.content = message
                    .content
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::VideoUrl { video_url } => ContentPart::Text {
                            text: format!("[video: {video_url}]"),
                        },
                        other => other,
                    })
                    .collect();
                message
            })
            .collect()
    }
}

/// Replaces media content parts the model does not support with
/// placeholder text carrying the archive path and recovery guidance
/// (multimodal spec US4.1/US4.2, architecture §5.1–5.2).
///
/// - `image_in == false` → [`ContentPart::ImageUrl`] ⇒ placeholder.
/// - `video_in == false` → [`ContentPart::VideoUrl`] ⇒ placeholder.
/// - `audio_in == false` → [`ContentPart::AudioUrl`] ⇒ placeholder.
///
/// ADR-003 revision: an undeclared capability is treated as unsupported,
/// so any unlisted modality is projected to a placeholder.
///
/// The projection is pure: context memory is never rewritten — only the
/// request copy (ADR-002). File parts are not provider media; they stay.
pub struct MediaCapabilityAdapter {
    caps: ModelCapability,
}

impl MediaCapabilityAdapter {
    /// Create an adapter bound to the given model capabilities.
    pub fn new(caps: ModelCapability) -> Self {
        Self { caps }
    }
}

impl ProjectionStep for MediaCapabilityAdapter {
    fn name(&self) -> &str {
        "media_capability_adapter"
    }

    fn applies_to(&self, _projection: &str) -> bool {
        !self.caps.image_in || !self.caps.video_in || !self.caps.audio_in
    }

    fn process(&self, messages: Vec<Message>) -> Vec<Message> {
        messages
            .into_iter()
            .map(|mut message| {
                message.content = message
                    .content
                    .into_iter()
                    .map(|part| self.adapt_part(part))
                    .collect();
                message
            })
            .collect()
    }
}

impl MediaCapabilityAdapter {
    fn adapt_part(&self, part: ContentPart) -> ContentPart {
        let cause = match &part {
            ContentPart::ImageUrl { image_url, .. } if !self.caps.image_in => {
                PlaceholderCause::CapabilityGap {
                    kind: MediaKind::Image,
                    path: image_url.id.as_deref(),
                }
            }
            ContentPart::VideoUrl { .. } if !self.caps.video_in => {
                PlaceholderCause::CapabilityGap {
                    kind: MediaKind::Video,
                    path: None,
                }
            }
            ContentPart::AudioUrl { .. } if !self.caps.audio_in => {
                PlaceholderCause::CapabilityGap {
                    kind: MediaKind::Audio,
                    path: None,
                }
            }
            _ => return part,
        };
        ContentPart::Text {
            text: build_media_placeholder(cause),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_placeholder::build_media_placeholder;
    use crate::message::{ImageUrlData, Message, Role};

    fn user_message(parts: Vec<ContentPart>) -> Message {
        Message {
            role: Role::User,
            name: None,
            content: parts,
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        }
    }

    fn text_part(text: &str) -> ContentPart {
        ContentPart::Text {
            text: text.to_string(),
        }
    }

    fn video_part(url: &str) -> ContentPart {
        ContentPart::VideoUrl {
            video_url: url.to_string(),
        }
    }

    fn audio_part(url: &str) -> ContentPart {
        ContentPart::AudioUrl {
            audio_url: url.to_string(),
        }
    }

    fn image_part(url: &str) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: url.to_string(),
                detail: None,
                id: None,
            },
            detail: None,
        }
    }

    fn image_part_with_id(id: Option<&str>) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: id.map(str::to_string),
            },
            detail: None,
        }
    }

    #[test]
    fn should_return_messages_unchanged_when_pipeline_has_no_steps() {
        // Given an empty pipeline and one message
        let pipeline = MessagePipeline::new();
        let message = user_message(vec![text_part("hello")]);
        // When processing for any projection
        let result = pipeline.run("normal", vec![message.clone()]);
        // Then the message is returned unchanged
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, vec![text_part("hello")]);
    }

    #[test]
    fn should_increase_step_count_and_names_when_registering_a_step() {
        // Given an empty pipeline
        let mut pipeline = MessagePipeline::new();
        assert_eq!(pipeline.step_count(), 0);
        // When registering a video resolver
        pipeline.register(Box::new(VideoReferenceResolver::new(false)));
        // Then the step is counted and named
        assert_eq!(pipeline.step_count(), 1);
        assert_eq!(
            pipeline.step_names(),
            vec!["video_reference_resolver".to_string()]
        );
    }

    #[test]
    fn should_run_steps_in_registration_order_when_processing() {
        // Given a pipeline with a no-op-counting pair of steps that both apply
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(VideoReferenceResolver::new(false)));
        pipeline.register(Box::new(MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: true,
            dynamically_loaded_tools: false,
        })));
        // And a message with a video part
        let message = user_message(vec![text_part("hi"), video_part("clip.mp4")]);
        // When processing
        let result = pipeline.run("normal", vec![message]);
        // Then the video was converted to text by the resolver (so the adapter
        // sees no video to strip) and the text remains
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].content,
            vec![text_part("hi"), text_part("[video: clip.mp4]")]
        );
    }

    #[test]
    fn should_skip_step_when_not_applicable_to_projection() {
        // Given a pipeline with a video resolver that does NOT apply (supports video)
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(VideoReferenceResolver::new(true)));
        // And a message with a video part
        let message = user_message(vec![video_part("clip.mp4")]);
        // When processing
        let result = pipeline.run("normal", vec![message.clone()]);
        // Then the video part is untouched
        assert_eq!(result[0].content, vec![video_part("clip.mp4")]);
    }

    #[test]
    fn should_apply_media_adapter_when_capability_is_undeclared() {
        // Given 能力未定义的模型（ADR-003 修订：未定义=不支持，全模态 false）
        // When 管道处理含图片的消息
        // Then 适配器生效：图片被投影为占位文本，不再原样放行
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(MediaCapabilityAdapter::new(
            ModelCapability::default(),
        )));
        // And a message with an image part
        let message = user_message(vec![image_part("pic.png")]);
        // When processing for any projection
        let result = pipeline.run("normal", vec![message]);
        // Then the adapter applies: the image is replaced with a placeholder
        assert_eq!(result[0].content.len(), 1);
        assert!(matches!(result[0].content[0], ContentPart::Text { .. }));
    }

    #[test]
    fn should_apply_media_adapter_when_capability_is_known_and_limited() {
        // Given a pipeline whose adapter is bound to a KNOWN text-only model
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        })));
        // And a message with an image part
        let message = user_message(vec![image_part("pic.png")]);
        // When processing for any projection
        let result = pipeline.run("normal", vec![message]);
        // Then the adapter applies: the image is replaced, not passed through
        assert_eq!(result[0].content.len(), 1);
        assert!(matches!(result[0].content[0], ContentPart::Text { .. }));
    }

    #[test]
    fn should_replace_video_with_text_placeholder_when_model_lacks_video_support() {
        // Given a resolver for a model without video support
        let resolver = VideoReferenceResolver::new(false);
        let message = user_message(vec![text_part("see this"), video_part("movie.mp4")]);
        // When processing
        let result = resolver.process(vec![message]);
        // Then the video part becomes a text placeholder
        assert_eq!(
            result[0].content,
            vec![text_part("see this"), text_part("[video: movie.mp4]")]
        );
    }

    #[test]
    fn should_leave_video_untouched_when_model_supports_video() {
        // Given a resolver for a model that supports video
        let resolver = VideoReferenceResolver::new(true);
        let message = user_message(vec![video_part("movie.mp4")]);
        // When processing
        let result = resolver.process(vec![message.clone()]);
        // Then the video part is preserved
        assert_eq!(result[0].content, vec![video_part("movie.mp4")]);
    }

    #[test]
    fn should_resolve_video_to_placeholder_when_capability_is_undeclared() {
        // Given 能力未定义的模型（ADR-003 修订：未定义=不支持，video_in=false）
        // When 视频引用解析步骤运行
        // Then VideoUrl 被替换为文本引用，不再原样放行
        let resolver = VideoReferenceResolver::for_capability(&ModelCapability::default());
        let message = user_message(vec![video_part("movie.mp4")]);
        let result = resolver.process(vec![message]);
        assert_eq!(result[0].content, vec![text_part("[video: movie.mp4]")]);
    }

    #[test]
    fn should_apply_video_resolver_when_capability_is_undeclared() {
        // Given 能力未定义的模型（ADR-003 修订：未定义=不支持，video_in=false）
        // When 管道按 applies_to 决定是否运行该步骤
        // Then 步骤被应用（applies_to=true），视频被替换为文本引用
        let resolver = VideoReferenceResolver::for_capability(&ModelCapability::default());
        assert!(resolver.applies_to("normal"));
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(resolver));
        let message = user_message(vec![video_part("movie.mp4")]);
        let result = pipeline.run("normal", vec![message]);
        assert_eq!(result[0].content, vec![text_part("[video: movie.mp4]")]);
    }

    #[test]
    fn should_replace_video_when_capability_known_without_video() {
        // Given 不支持视频的模型（video_in=false）
        // When 视频引用解析步骤运行
        // Then VideoUrl 仍被替换为文本引用（回归保护）
        let resolver = VideoReferenceResolver::for_capability(&ModelCapability::default());
        assert!(resolver.applies_to("normal"));
        let message = user_message(vec![text_part("see"), video_part("movie.mp4")]);
        let result = resolver.process(vec![message]);
        assert_eq!(
            result[0].content,
            vec![text_part("see"), text_part("[video: movie.mp4]")]
        );
    }

    #[test]
    fn should_replace_unsupported_media_with_placeholder_not_drop() {
        // Given 已知不支持图片的模型与含图+文的用户消息
        // When 能力适配投影
        // Then 图片部件被替换为占位文本（含省略声明），文字保留（US4.1）
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        });
        let messages = vec![user_message(vec![
            text_part("hello"),
            image_part_with_id(Some("/blobs/ab12")),
        ])];
        let out = adapter.process(messages);
        let parts = &out[0].content;
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], text_part("hello"));
        let ContentPart::Text { text } = &parts[1] else {
            panic!("图片应替换为文本")
        };
        assert!(text.contains("does not support image input"));
        assert!(text.contains("/blobs/ab12"));
    }

    #[test]
    fn should_replace_media_with_placeholder_when_capability_is_undeclared() {
        // Given 能力未定义的模型（ADR-003 修订：未定义=不支持，全模态 false）
        // When 能力适配投影
        // Then 媒体被投影为占位文本，不再原样放行
        let adapter = MediaCapabilityAdapter::new(ModelCapability::default());
        let messages = vec![user_message(vec![image_part_with_id(None)])];
        let out = adapter.process(messages);
        let ContentPart::Text { text } = &out[0].content[0] else {
            panic!("图片应替换为文本占位");
        };
        assert!(text.contains("does not support image input"));
    }

    #[test]
    fn should_replace_image_with_placeholder_when_model_lacks_image_support() {
        // Given an adapter for a model without image support
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        });
        let message = user_message(vec![text_part("a"), image_part("pic.png"), text_part("b")]);
        // When processing
        let result = adapter.process(vec![message]);
        // Then the image part is replaced with an omission placeholder and
        // both text parts remain
        let parts = &result[0].content;
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], text_part("a"));
        assert_eq!(parts[2], text_part("b"));
        let ContentPart::Text { text } = &parts[1] else {
            panic!("图片应替换为文本占位")
        };
        assert!(text.contains("does not support image input"));
    }

    #[test]
    fn should_replace_video_and_audio_with_placeholders_when_model_lacks_their_support() {
        // Given an adapter for a model that only supports images
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: true,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        });
        let message = user_message(vec![
            image_part("pic.png"),
            video_part("clip.mp4"),
            audio_part("song.mp3"),
            text_part("note"),
        ]);
        // When processing
        let result = adapter.process(vec![message]);
        // Then the image and text survive untouched, and the video/audio are
        // replaced with omission placeholders (not dropped)
        let parts = &result[0].content;
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], image_part("pic.png"));
        assert_eq!(parts[3], text_part("note"));
        let ContentPart::Text { text } = &parts[1] else {
            panic!("视频应替换为文本占位")
        };
        assert!(text.contains("does not support video input"));
        let ContentPart::Text { text } = &parts[2] else {
            panic!("音频应替换为文本占位")
        };
        assert!(text.contains("does not support audio input"));
    }

    #[test]
    fn should_preserve_all_media_parts_when_model_supports_everything() {
        // Given an adapter for a model that supports all media
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: true,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        });
        let message = user_message(vec![
            image_part("pic.png"),
            video_part("clip.mp4"),
            audio_part("song.mp3"),
            text_part("note"),
        ]);
        // When processing
        let result = adapter.process(vec![message.clone()]);
        // Then nothing is stripped
        assert_eq!(result[0].content, message.content);
    }

    #[test]
    fn should_keep_think_and_text_when_image_is_replaced_by_placeholder() {
        // Given an adapter that replaces all media
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        });
        let message = user_message(vec![
            ContentPart::Think {
                text: "reasoning".to_string(),
            },
            image_part("pic.png"),
            text_part("answer"),
        ]);
        // When processing
        let result = adapter.process(vec![message]);
        // Then the think part and text survive; the image is replaced with a
        // placeholder (not dropped, so the model can still account for it)
        let parts = &result[0].content;
        assert_eq!(
            parts.len(),
            3,
            "the image becomes a placeholder part, not a removal"
        );
        assert_eq!(
            parts[0],
            ContentPart::Think {
                text: "reasoning".to_string()
            }
        );
        assert_eq!(parts[2], text_part("answer"));
        let ContentPart::Text { text } = &parts[1] else {
            panic!("图片应替换为文本占位")
        };
        assert!(text.contains("does not support image input"));
    }

    #[test]
    fn should_replace_media_with_placeholder_matching_the_unified_constructor() {
        // Given an adapter for a known text-only model and a message with a
        // path-bearing image part
        let adapter = MediaCapabilityAdapter::new(ModelCapability {
            image_in: false,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        });
        let message = user_message(vec![image_part_with_id(Some("/tmp/blobs/ab12"))]);
        // When processing
        let result = adapter.process(vec![message]);
        // Then the placeholder text is byte-identical to the unified
        // media-placeholder constructor (multimodal spec §5.2 — one
        // constructor, all omission causes)
        let ContentPart::Text { text } = &result[0].content[0] else {
            panic!("图片应替换为文本占位")
        };
        assert_eq!(
            text,
            &build_media_placeholder(crate::media_placeholder::PlaceholderCause::CapabilityGap {
                kind: crate::media_placeholder::MediaKind::Image,
                path: Some("/tmp/blobs/ab12"),
            })
        );
    }

    #[test]
    fn should_apply_resolver_before_adapter_when_processing_mixed_media() {
        // Given a model that supports neither video nor audio
        let caps = ModelCapability {
            image_in: true,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        let mut pipeline = MessagePipeline::new();
        pipeline.register(Box::new(VideoReferenceResolver::new(caps.video_in)));
        pipeline.register(Box::new(MediaCapabilityAdapter::new(caps)));
        // And a message mixing media
        let message = user_message(vec![
            text_part("intro"),
            video_part("clip.mp4"),
            audio_part("song.mp3"),
            image_part("pic.png"),
        ]);
        // When processing
        let result = pipeline.run("normal", vec![message]);
        // Then video becomes the resolver's text reference, audio becomes the
        // omission placeholder, image and text remain
        let parts = &result[0].content;
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], text_part("intro"));
        assert_eq!(parts[1], text_part("[video: clip.mp4]"));
        assert_eq!(parts[3], image_part("pic.png"));
        let ContentPart::Text { text } = &parts[2] else {
            panic!("音频应替换为文本占位")
        };
        assert!(text.contains("does not support audio input"));
    }
}
