# Multimodal access via sub-agent model dispatch

Not every model supports image, video, or audio input. If you primarily use a fast text-only model for the main Agent but occasionally need to analyze screenshots, design references, or video recordings, you don't have to switch your entire session to a multimodal model. Instead, you can let the main Agent **dispatch a sub-agent that runs on a multimodal model**, keeping the text-only model for its speed and cost advantages while delegating media analysis on demand.

## How it works

When a user sends an image (or video/audio) to a session whose main model lacks the corresponding input capability, Mirri Code CLI automatically:

1. **Persists the media to a local file** before the message enters the conversation history, so the raw bytes are never lost.
2. **Replaces the media part with a downgrade placeholder** in the messages sent to the model. The placeholder tells the LLM what kind of media was dropped and where the file was saved.
3. **Suggests concrete next steps** in the placeholder text — specifically, dispatching a sub-agent with a multimodal model to read the file via the `ReadMediaFile` tool.

The LLM sees the placeholder and can act on it autonomously: it calls the `Agent` tool with a `model` parameter pointing to a vision-capable model, instructs the sub-agent to read the persisted file, and relays the analysis back to the user — without you manually switching models mid-session.

## Prerequisites

### 1. Configure at least one multimodal model

In `config.toml`, declare a model with `image_in` (and/or `video_in` / `audio_in`) in its `capabilities`:

```toml
[models."sonnet"]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_context_size = 200000
description = "balanced, supports vision"
capabilities = ["image_in", "tool_use"]

[models."gpt-4o"]
provider = "openai"
model = "gpt-4o"
max_context_size = 128000
description = "fast multimodal, supports image and video"
capabilities = ["image_in", "video_in", "tool_use"]
```

The `description` field is important: models with a `description` are exposed in the `Agent` and `AgentSwarm` tool descriptions as available overrides, so the LLM knows which models it can pass to sub-agents. Models without `description` are still valid for `defaultModel` but won't appear in the LLM-facing model list.

For most managed models (e.g. via `/login-with-kimi`), capabilities like `image_in` are inferred automatically from the model name — you don't need to declare them manually. You only need explicit `capabilities` when using a custom provider or a model whose name doesn't match known prefixes.

### 2. (Optional) Create a dedicated sub-agent profile

The built-in `coder` profile works for media analysis because it inherits `ReadMediaFile` from the parent agent. But if you want a lighter profile that focuses on media analysis without file-editing tools, create a custom one:

```yaml
# ~/.mirri-code/agents/media-reader.yaml
extends: agent
name: media-reader
description: Read media library, can read image files
promptVars:
  roleAdditional: >-
    You are now running as a subagent. All the `user` messages are sent by the
    main agent. The main agent cannot see your context, it can only see your
    last message when you finish the task. You must treat the parent agent as
    your caller. Do not directly ask the end user questions. If something is
    unclear, explain the ambiguity in your final summary to the parent agent.


    You are a **Multimodal Media Library Interpreter**. Unlike a typical
    code/file agent, you are equipped with native **vision, audio, and
    document-understanding capabilities**. Your primary mission is to "read" and
    "comprehend" the actual content inside the user's media library, rather than
    just staring at filenames and byte sizes. The main agent dispatches you
    precisely because it lacks eyes and ears for media files.


    **Your core philosophy:**

    - **Semantic over Statistic**: Prioritize understanding *what* the content
    is about (e.g., "a screenshot of a dashboard", "a podcast interview", "a cat
    video", "a scanned contract") over pure technical specs (resolution,
    bitrate).

    - **Technical specs as context**: Use read-only system tools (`ls`, `find`,
    `ffprobe`, `exiftool`, `mediainfo`) only to efficiently navigate the
    directory structure, count files, and catch obvious technical red flags
    (corrupted/empty files) before diving into multimodal analysis.


    **Execution Strategy (Speed & Cost Awareness):**

    1. **Discovery Phase (Read-only Bash/Glob)**: First, run `ls -R` / `find` to
    map the directory tree and extensions. Identify if there is a pre-made
    manifest (`.json`, `.xml`, `.nfo`). If the library is massive (>500 files),
    **do NOT analyze every file multimodally**—instead:
       - Sample ~5-10 representative files per folder/extension to infer the library's theme.
       - Use technical metadata (`ffprobe`/`exiftool`) to generate aggregate histograms (e.g., "80% are 1080p") for the rest.
    2. **Deep Dive Phase (Multimodal Analysis)**: For the selected samples,
    actively use your vision/audio/document-reading abilities to extract
    *actionable insights* that pure metadata cannot provide (e.g., "This folder
    contains holiday photos, but 3 of them are actually screenshots of plane
    tickets — be careful with privacy").

    3. **Anomaly Detection**: Flag files that are empty, unreadable, or
    semantically mismatched (e.g., a `.mp3` file that actually contains a spoken
    audiobook vs. a `.mp3` that contains instrumental music).


    **Tool Usage Rules:**

    - Use `Bash` **ONLY** for read-only operations (`ls`, `find`, `du`, `stat`,
    `file`, `ffprobe -v quiet -print_format json`, `exiftool -j`). **NEVER**
    modify, delete, or transcode.

    - Use `Read` for text-based sidecars/manifests.

    - Do **NOT** rely solely on external tools for interpretation; your built-in
    multimodal model is your primary analytical brain. Use tools merely to fetch
    raw bytes and directory lists.


    **Reporting to Parent Agent:**

    When the main agent dispatches you, its prompt specifies what it needs —
    follow that instruction on level of detail and format. If it didn't specify
    clearly, use your judgment: prioritize the most relevant information for
    the task at hand, and state any assumptions you made.


    Remember: You are the main agent's "eyes and ears". Be descriptive,
    interpretative, and efficient. If a file is too large to process directly,
    sample it or summarize based on available segments. Do not hallucinate — if
    you are unsure of a file's content, state the uncertainty clearly in the
    summary.
tools:
  - Bash
  - Read
  - ReadMediaFile
  - Glob
  - Grep
  - WebSearch
  - FetchURL
whenToUse: >-
  when the main agent have no ability to read/understand user inputted media
  library, main agent can dispatch this agent to know the overview of media
  library.
defaultModel: media
```

This profile:
- Inherits the system prompt template from `agent`
- Uses `gpt-4o` as the default model (so the main Agent doesn't need to pass `model` explicitly)
- Has a minimal tool set (no `Edit` or `Write`)
- Appears in the `Agent` tool's sub-agent list automatically

## Usage flow

Once configured, the flow is fully automatic from the user's perspective:

1. **User pastes an image** into the session (drag-and-drop, clipboard paste, or file path).
2. The main Agent's text-only model receives a placeholder like:

   ```
   [image omitted: current model has no image input]
   The original image has been saved to: /home/user/.mirri-code/media-originals/abc123.png
   To analyze this image, try one of these approaches:
   1. Dispatch a sub-agent with a multimodal model (by setting the "model" parameter
      to a vision-capable model such as claude-sonnet-4 or gpt-4o) and instruct it
      to read the file via ReadMediaFile.
   2. If no multimodal model is available, tell the user you cannot process the image
      and suggest they switch to a model with image input capability, or describe the
      image content in text so you can help.
   ```

3. The LLM calls the `Agent` tool:

   ```json
   {
     "subagent_type": "media-reader",
     "prompt": "Read the image at /home/user/.mirri-code/media-originals/abc123.png and describe what you see.",
     "model": "gpt-4o"
   }
   ```

4. The sub-agent reads the file, analyzes it, and returns a text description to the main Agent.

5. The main Agent incorporates the analysis and responds to the user — all without leaving the text-only session.

::: tip
If you set `defaultModel` on a custom profile (as in the `media-reader` example above), the LLM doesn't even need to pass the `model` parameter — the profile's default model is used automatically.
:::

## Tips

- **Cost control**: each sub-agent invocation consumes tokens from the multimodal model's quota. For simple "what's in this image" questions, one dispatch is enough. For iterative visual analysis (e.g. comparing screenshots), consider switching the main session to a multimodal model instead.
- **Privacy**: persisted media files are stored locally under the session directory. They are never uploaded to any server beyond the model provider's inference API. See [Data Locations](../configuration/data-locations.md) for the exact path.
- **Fallback**: if no multimodal model is configured, the placeholder advises the LLM to tell the user it cannot process the media and suggest switching models or describing the content in text.

## Related

- [Agents and Subagents](../customization/agents.md) — full reference for custom agent profiles, model selection, and sub-agent dispatch
- [Providers and Models](../configuration/providers.md) — how to configure model capabilities like `image_in` in `config.toml`
