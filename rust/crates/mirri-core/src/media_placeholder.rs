//! Unified media placeholder constructor — multimodal spec architecture
//! §5.2. One constructor, three causes (capability gap / size limit /
//! format rejected); the compression-offload cause belongs to E8 and is
//! out of scope for this batch.
//!
//! Path validity (US2.2): only filesystem-shaped paths are trusted as
//! archive handles — engine-internal protocol refs (`blobref:` /
//! `fileref:` / `ms://`) and remote URLs never enter the placeholder.

/// The modality of the omitted media part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }
}

/// Why the media part is being replaced with text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderCause<'a> {
    /// E4 capability gap — the model lacks the modality (known state).
    CapabilityGap {
        kind: MediaKind,
        path: Option<&'a str>,
    },
    /// E6 request-size limit — the 413 degraded/stripped projections.
    SizeLimit {
        kind: MediaKind,
        path: Option<&'a str>,
    },
    /// E6 rejected image format/encoding.
    FormatRejected {
        kind: MediaKind,
        path: Option<&'a str>,
    },
}

/// Trust only filesystem-shaped paths as archive handles (US2.2).
pub fn is_archive_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains("://")
        && !path.starts_with("blobref:")
        && !path.starts_with("fileref:")
        && !path.starts_with("ms:")
        && (path.contains('/') || path.contains('\\'))
}

fn valid_path<'a>(path: &'a Option<&'a str>) -> Option<&'a str> {
    path.filter(|p| is_archive_path(p))
}

/// Build the replacement text for an omitted media part.
pub fn build_media_placeholder(cause: PlaceholderCause<'_>) -> String {
    let kind = kind_of(&cause).as_str();
    let path = match &cause {
        PlaceholderCause::CapabilityGap { path, .. }
        | PlaceholderCause::SizeLimit { path, .. }
        | PlaceholderCause::FormatRejected { path, .. } => valid_path(path),
    };
    match cause {
        PlaceholderCause::CapabilityGap { .. } => match path {
            Some(path) => format!(
                "[{kind} omitted: the current model does not support {kind} input. \
The original {kind} has been saved to: {path}\n\
To still use this content, pick one of these options:\n\
1. Dispatch a dedicated multimodal subagent (its default model supports {kind} input — \
no model override needed) and instruct it to ReadMediaFile the file above.\n\
2. Dispatch a general subagent with its model parameter set to a model that supports \
{kind} input (see the available models list in the Agent tool description) and \
instruct it to ReadMediaFile the file above.\n\
3. If no multimodal model is available, tell the user you cannot view this {kind} \
and suggest switching to a {kind}-capable model or describing the content in text.]"
            ),
            None => format!("[{kind} omitted: the current model does not support {kind} input.]"),
        },
        PlaceholderCause::SizeLimit { .. } => match path {
            Some(path) => format!(
                "[{kind} omitted to fit the provider request size limit; the original \
has been saved to: {path}. Re-read the file with ReadMediaFile to view it.]"
            ),
            None => format!("[{kind} omitted: dropped to fit the provider request size limit.]"),
        },
        PlaceholderCause::FormatRejected { .. } => match path {
            Some(path) => format!(
                "[{kind} omitted: the provider rejected its format; the original \
has been saved to: {path}. Convert it to PNG/JPEG/WebP (e.g. via Bash) and \
re-attach it.]"
            ),
            None => format!("[{kind} omitted: the provider rejected its format.]"),
        },
    }
}

fn kind_of(cause: &PlaceholderCause<'_>) -> MediaKind {
    match cause {
        PlaceholderCause::CapabilityGap { kind, .. }
        | PlaceholderCause::SizeLimit { kind, .. }
        | PlaceholderCause::FormatRejected { kind, .. } => *kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_capability_gap_placeholder_with_path_and_recovery_paths() {
        // Given 能力缺口成因且留档路径有效
        // When 构造占位文本
        // Then 含省略声明、路径、三条恢复指引（US4.1/US5.1）
        let text = build_media_placeholder(PlaceholderCause::CapabilityGap {
            kind: MediaKind::Image,
            path: Some("/home/u/.mirri-code/sessions/s1/blobs/ab12"),
        });
        assert!(text.contains("does not support image input"));
        assert!(text.contains("saved to: /home/u/.mirri-code/sessions/s1/blobs/ab12"));
        assert!(text.contains("Dispatch a dedicated multimodal subagent"));
        assert!(text.contains("model parameter set to a model that supports image input"));
        assert!(text.contains("cannot view this image"));
    }

    #[test]
    fn should_build_capability_gap_placeholder_without_path_when_none() {
        // Given 能力缺口成因但无留档路径（纯网络地址入口）
        // When 构造占位文本
        // Then 仅简短省略声明，不编造路径（US4.1 末条）
        let text = build_media_placeholder(PlaceholderCause::CapabilityGap {
            kind: MediaKind::Image,
            path: None,
        });
        assert!(text.contains("does not support image input"));
        assert!(!text.contains("saved to"));
        assert!(!text.contains("Dispatch"));
    }

    #[test]
    fn should_build_capability_gap_placeholder_without_path_when_protocol_shaped() {
        // Given 路径为协议式标识（US2.2：不得误当作可读文件路径）
        // When 构造占位文本
        // Then 走无路径短文案
        for bogus in [
            "ms://att123",
            "blobref:image/png;ab12",
            "fileref:anthropic;f1",
        ] {
            let text = build_media_placeholder(PlaceholderCause::CapabilityGap {
                kind: MediaKind::Video,
                path: Some(bogus),
            });
            assert!(!text.contains(bogus), "协议标识 {bogus} 不得进入占位文本");
            assert!(text.contains("does not support video input"));
        }
    }

    #[test]
    fn should_build_size_limit_placeholder_with_reread_hint() {
        // Given 体积超限成因（413 降级链）
        // When 构造占位文本
        // Then 含路径与重读提示（US6.1）
        let text = build_media_placeholder(PlaceholderCause::SizeLimit {
            kind: MediaKind::Image,
            path: Some("/tmp/blobs/ab12"),
        });
        assert!(text.contains("size limit"));
        assert!(text.contains("saved to: /tmp/blobs/ab12"));
        assert!(text.contains("Re-read the file"));
    }

    #[test]
    fn should_build_format_rejected_placeholder_with_conversion_hint() {
        // Given 格式被拒成因
        // When 构造占位文本
        // Then 含路径与转换指引（US6.2）
        let text = build_media_placeholder(PlaceholderCause::FormatRejected {
            kind: MediaKind::Image,
            path: Some("/tmp/blobs/ab12"),
        });
        assert!(text.contains("format"));
        assert!(text.contains("Convert"));
    }

    #[test]
    fn should_reject_protocol_shaped_paths_in_is_archive_path() {
        // Given 各类协议式标识与合法文件路径
        // When 判定路径可采信性
        // Then 仅文件系统形态通过（US2.2）
        assert!(is_archive_path("/abs/path/blobs/ab12"));
        assert!(is_archive_path("rel/path/ab12"));
        assert!(!is_archive_path(""));
        assert!(!is_archive_path("ms://att123"));
        assert!(!is_archive_path("blobref:image/png;ab12"));
        assert!(!is_archive_path("fileref:anthropic;f1"));
        assert!(!is_archive_path("https://example.com/a.png"));
        assert!(!is_archive_path("plainname"));
    }
}
