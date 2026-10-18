//! Capability gap detection — detects when the current model cannot handle
//! a modality present in the user's message and builds a dispatch hint
//! suggesting a subagent with the required capability.
//!
//! Ports the TypeScript `capabilityGap` domain.

use std::collections::HashSet;
use std::fmt;

use crate::context::ContextMessage;
use crate::llm::ModelCapability as LlmModelCapability;
use crate::message::{ContentPart, Role};
use crate::model_catalog::{ModelCatalog, ModelCatalogEntry};
use crate::profile::ProfileManager;

// ---------------------------------------------------------------------------
// ModalityGap
// ---------------------------------------------------------------------------

/// A modality that may not be supported by the current model.
///
/// Each variant maps to a capability flag on a model profile and can be
/// detected in a slice of [`ContentPart`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalityGap {
    Image,
    Video,
    Audio,
}

impl ModalityGap {
    /// All modality gap variants in canonical order.
    pub const ALL: [Self; 3] = [Self::Image, Self::Video, Self::Audio];

    /// The capability field key on a model profile (e.g. `"image_in"`).
    pub fn capability_key(&self) -> &'static str {
        match self {
            Self::Image => "image_in",
            Self::Video => "video_in",
            Self::Audio => "audio_in",
        }
    }

    /// Human-readable label (e.g. `"image"`).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    /// Whether the given content parts contain this modality.
    pub fn detect(&self, parts: &[ContentPart]) -> bool {
        match self {
            Self::Image => {
                parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::ImageUrl { .. }))
                    || parts.iter().any(|p| match p {
                        ContentPart::Text { text } => image_notice_in_text(text),
                        _ => false,
                    })
            }
            Self::Video => parts
                .iter()
                .any(|p| matches!(p, ContentPart::VideoUrl { .. })),
            Self::Audio => parts
                .iter()
                .any(|p| matches!(p, ContentPart::AudioUrl { .. })),
        }
    }

    /// Whether the given capabilities support this modality.
    fn is_supported(&self, caps: &ModelCapabilities) -> bool {
        match self {
            Self::Image => caps.image_in,
            Self::Video => caps.video_in,
            Self::Audio => caps.audio_in,
        }
    }
}

impl fmt::Display for ModalityGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Whether a text part looks like a client-injected image omission notice
/// (US5.2): the client replaces an inline image with a text notice naming the
/// saved file, so the notice must count as "the prompt contains an image" for
/// gap detection.
fn image_notice_in_text(text: &str) -> bool {
    text.contains("[image omitted:")
        || text.contains("original image has been saved to")
        || (text.contains("Attached file") && {
            let lower = text.to_ascii_lowercase();
            [
                ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg", ".avif",
            ]
            .iter()
            .any(|ext| lower.contains(ext))
        })
}

// ---------------------------------------------------------------------------
// ModelCapabilities
// ---------------------------------------------------------------------------

/// Model capability flags relevant to modality gap detection.
///
/// This is the gap-detector-facing view of a model's capability profile. The
/// three modality flags (`image_in`, `video_in`, `audio_in`) drive gap
/// detection; `tool_use` and `thinking` mirror the corresponding flags of the
/// TypeScript `ModelCapability` contract so the type can grow into the single
/// canonical capability type.
///
/// **Relationship to [`crate::llm::ModelCapability`]:** the adapter-facing
/// `llm::ModelCapability` is what concrete [`Llm`](crate::llm::Llm) adapters
/// report (it carries the modality flags plus `dynamically_loaded_tools` but
/// not yet `tool_use`/`thinking`). Convert between them with
/// [`From<&LlmModelCapability>`](Self:From), which maps the modality flags and
/// conservatively leaves the flags the LLM type does not yet carry at `false`.
/// A full unification of the two types is deferred to a later phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    /// Whether the model can call tools (parity with the TS capability contract).
    pub tool_use: bool,
    /// Whether the model supports extended reasoning (parity with the TS
    /// capability contract).
    pub thinking: bool,
}

impl From<&LlmModelCapability> for ModelCapabilities {
    fn from(capability: &LlmModelCapability) -> Self {
        Self {
            image_in: capability.image_in,
            video_in: capability.video_in,
            audio_in: capability.audio_in,
            // `llm::ModelCapability` does not carry these flags yet; default
            // them to false until it does (see the type-level docs).
            tool_use: false,
            thinking: false,
        }
    }
}

// ---------------------------------------------------------------------------
// ProfileInfo
// ---------------------------------------------------------------------------

/// Information about an agent profile, used to find alternative models
/// that can handle modalities the current model cannot.
#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub name: String,
    pub default_model: String,
    pub model_capabilities: ModelCapabilities,
}

/// Build [`ProfileInfo`] entries from a [`ProfileManager`]'s registered
/// profiles.
///
/// Each registered profile becomes one entry carrying its name and default
/// model, so the gap detector can name concrete subagent profiles in its
/// dispatch hint.
///
/// **Model capabilities:** when a [`ModelCatalog`] is provided, each
/// profile's capabilities are resolved from the catalog (by alias id or raw
/// model name), so the detector can name profiles whose models genuinely
/// cover a modality gap. Without a catalog, capabilities default to
/// [`ModelCapabilities::default()`] (all modalities unsupported) — a
/// conservative stance that keeps hints silent until real capabilities are
/// known.
pub fn profile_infos_from_manager(
    manager: &ProfileManager,
    model_catalog: Option<&dyn ModelCatalog>,
) -> Vec<ProfileInfo> {
    manager
        .list()
        .into_iter()
        .filter_map(|name| {
            let profile = manager.get(&name)?;
            Some(ProfileInfo {
                name: profile.name,
                default_model: profile.model.clone(),
                model_capabilities: model_catalog
                    .and_then(|catalog| {
                        find_catalog_entry(catalog, &profile.model)
                            .map(|entry| capabilities_from_catalog_entry(&entry))
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Snapshot the model catalog's capabilities for gap-feasibility scanning.
/// An empty catalog (standalone flows) is treated as permissive by
/// [`CapabilityGapDetector::build_hint`].
pub fn catalog_capabilities(catalog: Option<&dyn ModelCatalog>) -> Vec<ModelCapabilities> {
    catalog
        .map(|c| {
            c.list_models()
                .iter()
                .map(capabilities_from_catalog_entry)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a model name (alias id or raw provider model id) to a catalog
/// entry, if the catalog knows it.
fn find_catalog_entry(catalog: &dyn ModelCatalog, model_name: &str) -> Option<ModelCatalogEntry> {
    catalog.get_by_id(model_name).or_else(|| {
        catalog
            .list_models()
            .into_iter()
            .find(|e| e.model == model_name)
    })
}

/// Convert catalog capability names to the gap detector's [`ModelCapabilities`].
///
/// Recognizes the canonical capability vocabulary (`"vision"`, `"video"`,
/// `"audio"`, `"tool-use"`, `"thinking"`) plus underscore variants; unknown
/// names are ignored.
fn capabilities_from_catalog_entry(entry: &ModelCatalogEntry) -> ModelCapabilities {
    let mut caps = ModelCapabilities::default();
    for name in &entry.capabilities {
        match name.as_str() {
            "vision" | "image" | "image_in" => caps.image_in = true,
            "video" | "video_in" => caps.video_in = true,
            "audio" | "audio_in" => caps.audio_in = true,
            "tool-use" | "tool_use" | "tool" => caps.tool_use = true,
            "thinking" | "reasoning" => caps.thinking = true,
            _ => {}
        }
    }
    caps
}

// ---------------------------------------------------------------------------
// CapabilityGapDetector
// ---------------------------------------------------------------------------

/// Static methods for detecting capability gaps and building subagent
/// dispatch hints.
pub struct CapabilityGapDetector;

impl CapabilityGapDetector {
    /// Detect all modality gaps: modalities present in `parts` that `caps`
    /// does not support.
    ///
    /// Returns gaps in canonical order (image, video, audio).
    pub fn detect_gaps(caps: &ModelCapabilities, parts: &[ContentPart]) -> Vec<ModalityGap> {
        ModalityGap::ALL
            .into_iter()
            .filter(|gap| gap.detect(parts) && !gap.is_supported(caps))
            .collect()
    }

    /// Whether any profile supports the given gap's modality.
    pub fn has_alternative_model(profiles: &[ProfileInfo], gap: &ModalityGap) -> bool {
        profiles
            .iter()
            .any(|p| gap.is_supported(&p.model_capabilities))
    }

    /// Names of profiles that support the given gap's modality, in the order
    /// they appear in `profiles`.
    pub fn profiles_for_capability(profiles: &[ProfileInfo], gap: &ModalityGap) -> Vec<String> {
        profiles
            .iter()
            .filter(|p| gap.is_supported(&p.model_capabilities))
            .map(|p| p.name.clone())
            .collect()
    }

    /// Build a hint string for the LLM when capability gaps exist.
    ///
    /// Three-layer fallback (TS `capabilityGapInjection.ts` parity, ADR-003
    /// revision wording): feasibility scans the model catalog (an empty
    /// snapshot is permissive — standalone flows cannot verify, so the hint
    /// is not suppressed); the primary wording is capability-oriented (point
    /// the LLM at the "Available models" list + the subagent `model`
    /// parameter so it judges for itself); covering profile names appear
    /// only as a parenthetical convenience. When nothing is feasible no
    /// hint is injected — the placeholder text's third recovery instruction
    /// is what tells the user.
    pub fn build_hint(
        gaps: &[ModalityGap],
        profiles: &[ProfileInfo],
        catalog: &[ModelCapabilities],
        attachment_paths: &[String],
    ) -> Option<String> {
        // Only include gaps that are feasible: an empty catalog snapshot is
        // permissive (can't verify — don't suppress, TS parity); otherwise a
        // profile's model or a catalog model must support the modality.
        let actionable: Vec<ModalityGap> = gaps
            .iter()
            .copied()
            .filter(|gap| {
                catalog.is_empty()
                    || Self::has_alternative_model(profiles, gap)
                    || catalog.iter().any(|caps| gap.is_supported(caps))
            })
            .collect();
        if actionable.is_empty() {
            return None;
        }

        let mut hint = String::new();
        let labels: Vec<&str> = actionable.iter().map(|g| g.label()).collect();
        let modality_word = if actionable.len() > 1 {
            "these modalities"
        } else {
            "this modality"
        };
        hint.push_str(&format!(
            "Your current model does not support {} input, but the user's prompt contains {modality_word}. \
To process the content, ",
            labels.join(", ")
        ));

        let dispatches: Vec<String> = actionable
            .iter()
            .map(|gap| {
                let label = gap.label();
                let covering: Vec<String> = Self::profiles_for_capability(profiles, gap)
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect();
                let parenthetical = if covering.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; e.g. the {} profiles' default models already support it",
                        covering.join(", ")
                    )
                };
                format!(
                    "{label}: dispatch a subagent and set its model parameter to a model that \
supports {label} input (see the \"Available models\" list in the Agent tool \
description{parenthetical})"
                )
            })
            .collect();
        hint.push_str(&dispatches.join("; "));
        hint.push('.');

        if !attachment_paths.is_empty() {
            let path_list: Vec<String> = attachment_paths
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect();
            let verb = if attachment_paths.len() > 1 {
                "s are"
            } else {
                " is"
            };
            hint.push_str(&format!(
                " The attachment{verb} saved at {} — instruct the subagent to ReadMediaFile on that path.",
                path_list.join(", ")
            ));
        }

        Some(hint)
    }
}

// ---------------------------------------------------------------------------
// extract_attachment_paths
// ---------------------------------------------------------------------------

/// Extract file paths from text content parts that match attachment patterns.
///
/// Scans for the TS `capabilityGapInjection.ts` marker shapes — `saved to:`,
/// `saved at:`, and `bytes):` — followed by an absolute path that may be
/// quoted (`saved at "/path"`) or unquoted and terminated by whitespace, a
/// quote, or an em-dash. Returns unique paths in order of first occurrence.
pub fn extract_attachment_paths(parts: &[ContentPart]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for part in parts {
        let ContentPart::Text { text } = part else {
            continue;
        };
        for marker in ["saved to", "saved at", "bytes):"] {
            let mut search_from = 0;
            while let Some(rel) = text[search_from..].find(marker) {
                let after_marker = search_from + rel + marker.len();
                search_from = after_marker;
                let Some(path) = scan_path(&text[after_marker..]) else {
                    continue;
                };
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// Scan one path after a marker: skip `:`/spaces; an optional opening quote
/// makes the path run to its closing twin, otherwise to the first of
/// whitespace / quote / em-dash. Only absolute paths count (TS parity).
fn scan_path(rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches([' ', ':', '\t']);
    let first = rest.chars().next()?;
    match first {
        '"' | '\'' => {
            let end = rest[1..].find(first)? + 1;
            let path = &rest[1..end];
            path.starts_with('/').then(|| path.to_string())
        }
        '/' => {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '—')
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// last_user_message
// ---------------------------------------------------------------------------

/// Find the last user message in a context message slice.
///
/// Returns `None` if no message with [`Role::User`] exists.
pub fn last_user_message(messages: &[ContextMessage]) -> Option<&ContextMessage> {
    messages.iter().rev().find(|m| m.message.role == Role::User)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageUrlData, Message};

    // -----------------------------------------------------------------------
    // ModalityGap::detect
    // -----------------------------------------------------------------------

    #[test]
    fn should_detect_image_when_image_url_part_present() {
        // Given content parts containing an image URL
        let parts = vec![
            ContentPart::Text {
                text: "Look at this".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "https://example.com/img.png".into(),
                    detail: None,
                    id: None,
                },
                detail: None,
            },
        ];

        // When detecting image modality
        let detected = ModalityGap::Image.detect(&parts);

        // Then image is detected
        assert!(detected);
    }

    #[test]
    fn should_detect_video_when_video_url_part_present() {
        // Given content parts containing a video URL
        let parts = vec![ContentPart::VideoUrl {
            video_url: "https://example.com/video.mp4".into(),
        }];

        // When detecting video modality
        let detected = ModalityGap::Video.detect(&parts);

        // Then video is detected
        assert!(detected);
    }

    #[test]
    fn should_detect_audio_when_audio_url_part_present() {
        // Given content parts containing an audio URL
        let parts = vec![ContentPart::AudioUrl {
            audio_url: "https://example.com/audio.mp3".into(),
        }];

        // When detecting audio modality
        let detected = ModalityGap::Audio.detect(&parts);

        // Then audio is detected
        assert!(detected);
    }

    #[test]
    fn should_not_detect_any_modality_when_only_text_present() {
        // Given content parts with only text
        let parts = vec![ContentPart::Text {
            text: "Just text".into(),
        }];

        // When detecting all modalities
        // Then none are detected
        assert!(!ModalityGap::Image.detect(&parts));
        assert!(!ModalityGap::Video.detect(&parts));
        assert!(!ModalityGap::Audio.detect(&parts));
    }

    #[test]
    fn should_detect_image_gap_from_client_text_notice() {
        // Given 用户消息只含客户端注入的文本通知（US5.2）
        // When 检测 image 模态
        // Then 文本通知被识别为 image 缺口信号
        let parts = vec![ContentPart::Text {
            text: "[image omitted: the current model does not support image input. \
The original image has been saved to: /tmp/a.png]"
                .into(),
        }];
        assert!(ModalityGap::Image.detect(&parts));
    }

    #[test]
    fn should_not_detect_image_gap_from_plain_text_without_notice() {
        // Given 普通文本（无图片部件、无省略通知）
        // When 检测 image 模态
        // Then 不误报
        let parts = vec![ContentPart::Text {
            text: "just a normal message".into(),
        }];
        assert!(!ModalityGap::Image.detect(&parts));
    }

    // -----------------------------------------------------------------------
    // ModalityGap::capability_key / label
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_correct_capability_key_when_called_on_each_modality() {
        // Given all modality gaps
        // When getting capability keys
        // Then keys match expected field names
        assert_eq!(ModalityGap::Image.capability_key(), "image_in");
        assert_eq!(ModalityGap::Video.capability_key(), "video_in");
        assert_eq!(ModalityGap::Audio.capability_key(), "audio_in");
    }

    #[test]
    fn should_return_correct_label_when_called_on_each_modality() {
        // Given all modality gaps
        // When getting labels
        // Then labels match expected names
        assert_eq!(ModalityGap::Image.label(), "image");
        assert_eq!(ModalityGap::Video.label(), "video");
        assert_eq!(ModalityGap::Audio.label(), "audio");
    }

    // -----------------------------------------------------------------------
    // CapabilityGapDetector::detect_gaps
    // -----------------------------------------------------------------------

    #[test]
    fn should_detect_image_gap_when_model_lacks_image_support() {
        // Given a model without image support and image content
        let caps = ModelCapabilities {
            image_in: false,
            video_in: true,
            audio_in: true,
            ..Default::default()
        };
        let parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "https://example.com/img.png".into(),
                detail: None,
                id: None,
            },
            detail: None,
        }];

        // When detecting gaps
        let gaps = CapabilityGapDetector::detect_gaps(&caps, &parts);

        // Then only image gap is detected
        assert_eq!(gaps, vec![ModalityGap::Image]);
    }

    #[test]
    fn should_detect_multiple_gaps_when_model_lacks_multiple_modalities() {
        // Given a model without image or video support, and content with both
        let caps = ModelCapabilities {
            image_in: false,
            video_in: false,
            audio_in: true,
            ..Default::default()
        };
        let parts = vec![
            ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "https://example.com/img.png".into(),
                    detail: None,
                    id: None,
                },
                detail: None,
            },
            ContentPart::VideoUrl {
                video_url: "https://example.com/video.mp4".into(),
            },
        ];

        // When detecting gaps
        let gaps = CapabilityGapDetector::detect_gaps(&caps, &parts);

        // Then both image and video gaps are detected (in canonical order)
        assert_eq!(gaps, vec![ModalityGap::Image, ModalityGap::Video]);
    }

    #[test]
    fn should_return_no_gaps_when_model_supports_all_modalities() {
        // Given a model with all capabilities and multimodal content
        let caps = ModelCapabilities {
            image_in: true,
            video_in: true,
            audio_in: true,
            ..Default::default()
        };
        let parts = vec![
            ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "https://example.com/img.png".into(),
                    detail: None,
                    id: None,
                },
                detail: None,
            },
            ContentPart::VideoUrl {
                video_url: "https://example.com/video.mp4".into(),
            },
        ];

        // When detecting gaps
        let gaps = CapabilityGapDetector::detect_gaps(&caps, &parts);

        // Then no gaps are detected
        assert!(gaps.is_empty());
    }

    #[test]
    fn should_return_no_gaps_when_content_is_text_only() {
        // Given a model with no capabilities and text-only content
        let caps = ModelCapabilities::default();
        let parts = vec![ContentPart::Text {
            text: "Just text".into(),
        }];

        // When detecting gaps
        let gaps = CapabilityGapDetector::detect_gaps(&caps, &parts);

        // Then no gaps are detected (text is always supported)
        assert!(gaps.is_empty());
    }

    // -----------------------------------------------------------------------
    // CapabilityGapDetector::has_alternative_model
    // -----------------------------------------------------------------------

    #[test]
    fn should_find_alternative_model_when_profile_supports_gap() {
        // Given profiles where one supports images
        let profiles = vec![
            ProfileInfo {
                name: "text-agent".into(),
                default_model: "gpt-4".into(),
                model_capabilities: ModelCapabilities::default(),
            },
            ProfileInfo {
                name: "vision-agent".into(),
                default_model: "gpt-4-vision".into(),
                model_capabilities: ModelCapabilities {
                    image_in: true,
                    ..Default::default()
                },
            },
        ];

        // When checking for an alternative for the image gap
        let has_alt = CapabilityGapDetector::has_alternative_model(&profiles, &ModalityGap::Image);

        // Then an alternative is found
        assert!(has_alt);
    }

    #[test]
    fn should_return_no_alternative_when_no_profile_supports_gap() {
        // Given profiles where none support video
        let profiles = vec![ProfileInfo {
            name: "text-agent".into(),
            default_model: "gpt-4".into(),
            model_capabilities: ModelCapabilities {
                image_in: true,
                ..Default::default()
            },
        }];

        // When checking for an alternative for the video gap
        let has_alt = CapabilityGapDetector::has_alternative_model(&profiles, &ModalityGap::Video);

        // Then no alternative is found
        assert!(!has_alt);
    }

    // -----------------------------------------------------------------------
    // CapabilityGapDetector::profiles_for_capability
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_supporting_profile_names_when_multiple_profiles_support_the_capability() {
        // Given profiles where two support audio
        let profiles = vec![
            ProfileInfo {
                name: "text-agent".into(),
                default_model: "gpt-4".into(),
                model_capabilities: ModelCapabilities::default(),
            },
            ProfileInfo {
                name: "audio-agent".into(),
                default_model: "whisper".into(),
                model_capabilities: ModelCapabilities {
                    audio_in: true,
                    ..Default::default()
                },
            },
            ProfileInfo {
                name: "multimodal-agent".into(),
                default_model: "gpt-4o".into(),
                model_capabilities: ModelCapabilities {
                    image_in: true,
                    audio_in: true,
                    ..Default::default()
                },
            },
        ];

        // When listing profiles for audio capability
        let names = CapabilityGapDetector::profiles_for_capability(&profiles, &ModalityGap::Audio);

        // Then both audio-capable profiles are listed (in order)
        assert_eq!(names, vec!["audio-agent", "multimodal-agent"]);
    }

    // -----------------------------------------------------------------------
    // CapabilityGapDetector::build_hint
    // -----------------------------------------------------------------------

    #[test]
    fn should_build_hint_when_gap_has_alternative_model() {
        // Given an image gap with an alternative profile and a covering catalog model
        let gaps = vec![ModalityGap::Image];
        let profiles = vec![ProfileInfo {
            name: "vision-agent".into(),
            default_model: "gpt-4-vision".into(),
            model_capabilities: ModelCapabilities {
                image_in: true,
                ..Default::default()
            },
        }];
        let catalog = vec![ModelCapabilities {
            image_in: true,
            ..Default::default()
        }];

        // When building a hint
        let hint = CapabilityGapDetector::build_hint(&gaps, &profiles, &catalog, &[]);

        // Then a capability-oriented hint is returned, naming the profile
        // only as a parenthetical convenience
        let hint = hint.expect("hint should be Some");
        assert!(hint.contains("does not support image input"));
        assert!(hint.contains("model that supports image input"));
        assert!(hint.contains("dispatch a subagent"));
        assert!(hint.contains("'vision-agent'"));
    }

    #[test]
    fn should_return_none_when_no_gap_has_alternative_model() {
        // Given an image gap with no alternative in profiles or the catalog
        let gaps = vec![ModalityGap::Image];
        let profiles = vec![ProfileInfo {
            name: "text-agent".into(),
            default_model: "gpt-4".into(),
            model_capabilities: ModelCapabilities::default(),
        }];
        let catalog = vec![ModelCapabilities::default()];

        // When building a hint
        let hint = CapabilityGapDetector::build_hint(&gaps, &profiles, &catalog, &[]);

        // Then no hint is returned
        assert!(hint.is_none());
    }

    #[test]
    fn should_include_attachment_paths_in_hint_when_available() {
        // Given a gap with an alternative model and attachment paths
        let gaps = vec![ModalityGap::Image];
        let profiles = vec![ProfileInfo {
            name: "vision-agent".into(),
            default_model: "gpt-4-vision".into(),
            model_capabilities: ModelCapabilities {
                image_in: true,
                ..Default::default()
            },
        }];
        let catalog = vec![ModelCapabilities {
            image_in: true,
            ..Default::default()
        }];
        let paths = vec!["/tmp/screenshot.png".into()];

        // When building a hint
        let hint = CapabilityGapDetector::build_hint(&gaps, &profiles, &catalog, &paths);

        // Then the hint includes the attachment path with ReadMediaFile guidance
        let hint = hint.expect("hint should be Some");
        assert!(hint.contains("/tmp/screenshot.png"));
        assert!(hint.contains("instruct the subagent to ReadMediaFile on that path"));
    }

    #[test]
    fn should_only_include_actionable_gaps_when_some_gaps_lack_alternative_models() {
        // Given two gaps where only one has an alternative model
        let gaps = vec![ModalityGap::Image, ModalityGap::Video];
        let profiles = vec![ProfileInfo {
            name: "vision-agent".into(),
            default_model: "gpt-4-vision".into(),
            model_capabilities: ModelCapabilities {
                image_in: true,
                ..Default::default()
            },
        }];
        let catalog = vec![ModelCapabilities {
            image_in: true,
            ..Default::default()
        }];

        // When building a hint
        let hint = CapabilityGapDetector::build_hint(&gaps, &profiles, &catalog, &[]);

        // Then the hint mentions image but not video
        let hint = hint.expect("hint should be Some");
        assert!(hint.contains("image"));
        assert!(!hint.contains("video"));
    }

    #[test]
    fn should_build_capability_oriented_hint_with_readmediafile_guidance() {
        // Given image 缺口且有可行目录模型（无匹配 profile）
        // When 构造提示
        // Then 主文案指向 Available models 清单与 model 参数，附 ReadMediaFile 指引，不含 profile 括注
        let gaps = vec![ModalityGap::Image];
        let catalog = vec![ModelCapabilities {
            image_in: true,
            ..Default::default()
        }];
        let hint =
            CapabilityGapDetector::build_hint(&gaps, &[], &catalog, &["/tmp/shot.png".into()])
                .expect("hint");
        assert!(hint.contains("does not support image input"));
        assert!(hint.contains("model that supports image input"));
        assert!(hint.contains("\"Available models\" list in the Agent tool description"));
        assert!(hint.contains("instruct the subagent to ReadMediaFile on that path"));
        assert!(!hint.contains("profile"));
    }

    #[test]
    fn should_append_profile_names_as_parenthetical_when_profiles_cover_gap() {
        // Given image 缺口且 media profile 覆盖
        // When 构造提示
        // Then 主文案仍是能力导向，profile 名作为括注便利信息出现
        let gaps = vec![ModalityGap::Image];
        let profiles = vec![ProfileInfo {
            name: "media".into(),
            default_model: "gpt-4o".into(),
            model_capabilities: ModelCapabilities {
                image_in: true,
                ..Default::default()
            },
        }];
        let hint = CapabilityGapDetector::build_hint(&gaps, &profiles, &[], &[]).expect("hint");
        assert!(hint.contains("model that supports image input"));
        assert!(hint.contains("'media'"));
    }

    #[test]
    fn should_not_suppress_hint_when_catalog_snapshot_is_empty() {
        // Given standalone 流无目录快照（无法验证可行性）
        // When 构造提示
        // Then 不压制——仍按可行处理（TS permissive 同款）
        let gaps = vec![ModalityGap::Image];
        let hint = CapabilityGapDetector::build_hint(&gaps, &[], &[], &[]);
        assert!(hint.is_some());
    }

    #[test]
    fn should_return_none_when_no_model_or_profile_covers_gap() {
        // Given image 缺口但目录与 profiles 均无支持模型
        // When 构造提示
        // Then 不注入（占位文本第③条负责告知用户）
        let gaps = vec![ModalityGap::Image];
        let catalog = vec![ModelCapabilities::default()];
        assert!(CapabilityGapDetector::build_hint(&gaps, &[], &catalog, &[]).is_none());
    }

    // -----------------------------------------------------------------------
    // extract_attachment_paths
    // ---------------------------------------------------------------------------

    #[test]
    fn should_extract_path_when_text_contains_saved_to_pattern() {
        // Given text containing "saved to: /path"
        let parts = vec![ContentPart::Text {
            text: "Screenshot saved to: /tmp/screenshot.png".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then the path is extracted
        assert_eq!(paths, vec!["/tmp/screenshot.png"]);
    }

    #[test]
    fn should_extract_path_when_text_contains_saved_at_pattern() {
        // Given text containing "saved at: /path"
        let parts = vec![ContentPart::Text {
            text: "File saved at: /home/user/file.txt".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then the path is extracted
        assert_eq!(paths, vec!["/home/user/file.txt"]);
    }

    #[test]
    fn should_extract_path_when_text_contains_bytes_pattern() {
        // Given text containing "bytes): /path"
        let parts = vec![ContentPart::Text {
            text: "Downloaded (1234 bytes): /tmp/download.bin".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then the path is extracted
        assert_eq!(paths, vec!["/tmp/download.bin"]);
    }

    #[test]
    fn should_return_unique_paths_when_duplicates_exist() {
        // Given text with the same path appearing twice
        let parts = vec![ContentPart::Text {
            text: "File saved to: /tmp/file.png\nAnother file saved to: /tmp/file.png".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then only one unique path is returned
        assert_eq!(paths, vec!["/tmp/file.png"]);
    }

    #[test]
    fn should_extract_all_paths_when_text_contains_multiple_patterns() {
        // Given text with multiple different patterns on separate lines
        let parts = vec![ContentPart::Text {
            text: "Image saved to: /tmp/img.png\nAudio (500 bytes): /tmp/audio.mp3".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then both paths are extracted in order
        assert_eq!(paths, vec!["/tmp/img.png", "/tmp/audio.mp3"]);
    }

    #[test]
    fn should_return_empty_when_no_patterns_match() {
        // Given text with no attachment patterns
        let parts = vec![ContentPart::Text {
            text: "Just a regular message with no file paths".into(),
        }];

        // When extracting attachment paths
        let paths = extract_attachment_paths(&parts);

        // Then no paths are returned
        assert!(paths.is_empty());
    }

    #[test]
    fn should_extract_quoted_path_when_saved_at_without_colon() {
        // Given 压缩说明里的引号式路径 saved at "/path"（TS 对齐）
        // When 提取附件路径
        // Then 引号内路径被提取，止于引号
        let parts = vec![ContentPart::Text {
            text: "The uncompressed original is saved at \"/tmp/media-originals/abc.png\"".into(),
        }];
        assert_eq!(
            extract_attachment_paths(&parts),
            vec!["/tmp/media-originals/abc.png".to_string()]
        );
    }

    #[test]
    fn should_terminate_unquoted_path_at_emdash() {
        // Given bytes): /path — open it 形态（kap-server 附带文件通知）
        // When 提取附件路径
        // Then 路径止于 em-dash，不含后续说明
        let parts = vec![ContentPart::Text {
            text:
                "Attached file \"a.png\" (image/png, 1234 bytes): /tmp/attachments/a.png — open it"
                    .into(),
        }];
        assert_eq!(
            extract_attachment_paths(&parts),
            vec!["/tmp/attachments/a.png".to_string()]
        );
    }

    // -----------------------------------------------------------------------
    // last_user_message
    // -----------------------------------------------------------------------

    #[test]
    fn should_find_last_user_message_when_history_contains_multiple_user_messages() {
        // Given messages with multiple user messages
        let messages = vec![
            ContextMessage::from_message(Message::user("First")),
            ContextMessage::from_message(Message::assistant("Response")),
            ContextMessage::from_message(Message::user("Second")),
        ];

        // When finding the last user message
        let last = last_user_message(&messages);

        // Then the last user message is found
        let last = last.expect("should find a user message");
        assert_eq!(last.message.text_content(), "Second");
    }

    #[test]
    fn should_return_none_when_no_user_message_exists() {
        // Given messages with no user messages
        let messages = vec![
            ContextMessage::from_message(Message::assistant("Hello")),
            ContextMessage::from_message(Message::system("System note")),
        ];

        // When finding the last user message
        let last = last_user_message(&messages);

        // Then None is returned
        assert!(last.is_none());
    }

    // -----------------------------------------------------------------------
    // From<&llm::ModelCapability>
    // -----------------------------------------------------------------------

    #[test]
    fn should_report_gap_when_capability_is_undeclared() {
        // Given 模型能力未定义（ADR-003 修订：未定义=不支持）
        // When 转换为注入器能力视图并检测缺口
        // Then image=false 即报告 image 缺口
        let llm_caps = crate::llm::ModelCapability::default();
        let gap_caps = ModelCapabilities::from(&llm_caps);
        assert!(!gap_caps.image_in);
        let gaps = CapabilityGapDetector::detect_gaps(
            &gap_caps,
            &[ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "data:image/png;base64,AA".into(),
                    detail: None,
                    id: None,
                },
                detail: None,
            }],
        );
        assert_eq!(gaps, vec![ModalityGap::Image]);
    }

    #[test]
    fn should_map_modality_flags_when_converting_from_llm_capability() {
        // Given an llm ModelCapability with image and audio input enabled
        let llm_caps = crate::llm::ModelCapability {
            image_in: true,
            video_in: false,
            audio_in: true,
            dynamically_loaded_tools: true,
        };

        // When converting to the gap detector's ModelCapabilities
        let caps = ModelCapabilities::from(&llm_caps);

        // Then the modality flags carry over
        assert!(caps.image_in);
        assert!(!caps.video_in);
        assert!(caps.audio_in);
    }

    #[test]
    fn should_default_tool_use_and_thinking_when_converting_from_llm_capability() {
        // Given an llm ModelCapability (which does not carry tool_use/thinking yet)
        let llm_caps = crate::llm::ModelCapability::default();

        // When converting to the gap detector's ModelCapabilities
        let caps = ModelCapabilities::from(&llm_caps);

        // Then tool_use and thinking are conservatively false
        assert!(!caps.tool_use);
        assert!(!caps.thinking);
    }

    // -----------------------------------------------------------------------
    // profile_infos_from_manager
    // -----------------------------------------------------------------------

    fn test_profile(name: &str, model: &str) -> crate::profile::AgentProfile {
        crate::profile::AgentProfile {
            name: name.into(),
            model: model.into(),
            provider: None,
            system_prompt: None,
            system_prompt_file: None,
            max_turns: None,
            max_steps_per_turn: None,
            permission_mode: None,
            tools: None,
            disallowed_tools: None,
            capabilities_required: None,
            thinking_effort: None,
            subagents: None,
            ..Default::default()
        }
    }

    #[test]
    fn should_build_one_profile_info_per_registered_profile_when_manager_has_profiles() {
        // Given a profile manager with two registered profiles
        let manager = crate::profile::ProfileManager::new();
        manager
            .register(test_profile("test-coder", "claude-sonnet-4-20250514"))
            .unwrap();
        manager
            .register(test_profile("test-vision", "gpt-4-vision"))
            .unwrap();

        // When building profile infos
        let infos = profile_infos_from_manager(&manager, None);

        // Then the registered profiles appear with correct name and model
        let coder = infos
            .iter()
            .find(|i| i.name == "test-coder")
            .expect("test-coder profile");
        assert_eq!(coder.default_model, "claude-sonnet-4-20250514");
        let vision = infos
            .iter()
            .find(|i| i.name == "test-vision")
            .expect("test-vision profile");
        assert_eq!(vision.default_model, "gpt-4-vision");
    }

    #[test]
    fn should_return_only_builtin_profiles_when_no_user_profiles_are_registered() {
        // Given a fresh profile manager (only builtin profiles)
        let manager = crate::profile::ProfileManager::new();

        // When building profile infos
        let infos = profile_infos_from_manager(&manager, None);

        // Then at least one builtin profile info is produced
        assert!(!infos.is_empty());
    }

    #[test]
    fn should_default_model_capabilities_when_no_model_catalog_available() {
        // Given a profile whose model capabilities are unknown (no model catalog)
        let manager = crate::profile::ProfileManager::new();
        manager
            .register(test_profile("test-coder", "claude-sonnet-4-20250514"))
            .unwrap();

        // When building profile infos
        let infos = profile_infos_from_manager(&manager, None);

        // Then capabilities default to all-unsupported (conservative)
        let caps = &infos
            .iter()
            .find(|i| i.name == "test-coder")
            .expect("test-coder profile")
            .model_capabilities;
        assert!(!caps.image_in);
        assert!(!caps.video_in);
        assert!(!caps.audio_in);
        assert!(!caps.tool_use);
        assert!(!caps.thinking);
    }

    // -----------------------------------------------------------------------
    // catalog-backed profile_infos_from_manager
    // -----------------------------------------------------------------------

    /// A fixed catalog for testing capability resolution.
    struct TestCatalog {
        entries: Vec<crate::model_catalog::ModelCatalogEntry>,
    }

    impl TestCatalog {
        fn new(entries: Vec<crate::model_catalog::ModelCatalogEntry>) -> Self {
            Self { entries }
        }
    }

    impl crate::model_catalog::ModelCatalog for TestCatalog {
        fn list_models(&self) -> Vec<crate::model_catalog::ModelCatalogEntry> {
            self.entries.clone()
        }

        fn get_by_id(&self, id: &str) -> Option<crate::model_catalog::ModelCatalogEntry> {
            self.entries.iter().find(|e| e.id == id).cloned()
        }
    }

    fn catalog_entry(
        id: &str,
        model: &str,
        capabilities: Vec<&str>,
    ) -> crate::model_catalog::ModelCatalogEntry {
        crate::model_catalog::ModelCatalogEntry {
            id: id.to_string(),
            model: model.to_string(),
            provider: "test".to_string(),
            description: None,
            max_context_size: None,
            capabilities: capabilities.into_iter().map(String::from).collect(),
            default_effort: None,
        }
    }

    #[test]
    fn should_fill_capabilities_from_catalog_when_profile_model_is_an_alias() {
        // Given a catalog whose alias "vision" maps to a vision-capable model
        let catalog = TestCatalog::new(vec![catalog_entry(
            "vision",
            "gpt-4-vision",
            vec!["vision", "tool-use"],
        )]);
        let manager = crate::profile::ProfileManager::new();
        manager.register(test_profile("vision", "vision")).unwrap();

        // When building profile infos with the catalog
        let infos = profile_infos_from_manager(&manager, Some(&catalog));

        // Then the profile's capabilities come from the catalog entry
        let caps = &infos
            .iter()
            .find(|i| i.name == "vision")
            .expect("vision profile")
            .model_capabilities;
        assert!(caps.image_in);
        assert!(caps.tool_use);
        assert!(!caps.video_in);
    }

    #[test]
    fn should_fill_capabilities_from_catalog_when_profile_model_is_a_raw_model_name() {
        // Given a catalog keyed by alias but whose entry model is the raw id
        let catalog = TestCatalog::new(vec![catalog_entry(
            "gpt4o",
            "gpt-4o",
            vec!["vision", "video", "audio", "thinking"],
        )]);
        let manager = crate::profile::ProfileManager::new();
        manager
            .register(test_profile("multimodal", "gpt-4o"))
            .unwrap();

        // When building profile infos with the catalog
        let infos = profile_infos_from_manager(&manager, Some(&catalog));

        // Then the raw model name matches the catalog entry's model field
        let caps = &infos
            .iter()
            .find(|i| i.name == "multimodal")
            .expect("multimodal profile")
            .model_capabilities;
        assert!(caps.image_in);
        assert!(caps.video_in);
        assert!(caps.audio_in);
        assert!(caps.thinking);
    }

    /// should inject capability gap hint when model lacks vision
    #[test]
    fn should_inject_capability_gap_hint_when_model_lacks_vision() {
        // Given a catalog in which the "vision" profile's model supports
        // images, while the current (parent) model does not
        let catalog = TestCatalog::new(vec![catalog_entry(
            "vision",
            "gpt-4-vision",
            vec!["vision"],
        )]);
        let manager = crate::profile::ProfileManager::new();
        manager.register(test_profile("vision", "vision")).unwrap();

        // When detecting a vision gap for the model-less parent and building
        // a hint from catalog-backed profile infos
        let parent_caps = ModelCapabilities::default();
        let parts = vec![ContentPart::ImageUrl {
            image_url: crate::message::ImageUrlData {
                url: "https://example.com/img.png".into(),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        let gaps = CapabilityGapDetector::detect_gaps(&parent_caps, &parts);
        let infos = profile_infos_from_manager(&manager, Some(&catalog));
        let snapshot = catalog_capabilities(Some(&catalog));
        let hint = CapabilityGapDetector::build_hint(&gaps, &infos, &snapshot, &[]);

        // Then a hint is injected naming the vision-capable profile
        let hint = hint.expect("hint should be Some when catalog knows a vision model");
        assert!(hint.contains("image"));
        assert!(hint.contains("'vision'"));
        assert!(hint.contains("dispatch a subagent"));
    }

    #[test]
    fn should_inject_gap_hint_when_catalog_is_absent() {
        // Given the same profile but no catalog (no feasibility snapshot —
        // standalone flow cannot verify)
        let manager = crate::profile::ProfileManager::new();
        manager
            .register(test_profile("vision", "gpt-4-vision"))
            .unwrap();
        let parent_caps = ModelCapabilities::default();
        let parts = vec![ContentPart::ImageUrl {
            image_url: crate::message::ImageUrlData {
                url: "https://example.com/img.png".into(),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        let gaps = CapabilityGapDetector::detect_gaps(&parent_caps, &parts);

        // When building a hint from unknown capabilities
        let infos = profile_infos_from_manager(&manager, None);
        let hint = CapabilityGapDetector::build_hint(&gaps, &infos, &[], &[]);

        // Then the hint is still injected (an empty catalog snapshot is
        // permissive — cannot verify, so the hint is not suppressed, TS parity)
        assert!(hint.is_some());
    }
}
