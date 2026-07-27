# 通过子 Agent 调度实现多模态访问

并非所有模型都支持图片、视频或音频输入。如果你主要使用一个快速的纯文本模型作为主 Agent，但偶尔需要分析截图、设计稿或视频录像，你不必把整个会话切换到多模态模型。你可以让主 Agent **派发一个运行在多模态模型上的子 Agent**，在保持纯文本模型的速度和成本优势的同时，按需委托媒体分析任务。

## 工作原理

当用户向主模型不支持对应输入能力的会话发送图片（或视频/音频）时，Mirri Code CLI 会自动：

1. **将媒体持久化到本地文件**，在消息进入对话历史之前完成，确保原始数据不会丢失。
2. **用降级占位符替换媒体部分**，占位符会告知 LLM 被省略的媒体类型和文件保存路径。
3. **在占位符文本中建议具体下一步操作**——具体来说，就是派发一个使用多模态模型的子 Agent，通过 `ReadMediaFile` 工具读取文件。

LLM 看到占位符后可以自主行动：它调用 `Agent` 工具，在 `model` 参数中指定一个支持视觉的模型，指示子 Agent 读取已保存的文件，然后将分析结果转述给用户——全程无需手动切换模型。

## 前置条件

### 1. 配置至少一个多模态模型

在 `config.toml` 中声明一个在 `capabilities` 中包含 `image_in`（和/或 `video_in` / `audio_in`）的模型：

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

`description` 字段很重要：设置了 `description` 的模型会出现在 `Agent` 和 `AgentSwarm` 工具描述的可用模型列表中，让 LLM 知道哪些模型可以传给子 Agent。没有设置 `description` 的模型仍可用于 `defaultModel`，但不会出现在 LLM 可见的模型列表中。

对于大多数托管模型（如通过 `/login-with-kimi` 登录的模型），`image_in` 等能力会从模型名称自动推断——你无需手动声明。只有在使用自定义 provider 或模型名称不匹配已知前缀时，才需要显式声明 `capabilities`。

### 2.（可选）创建专用子 Agent profile

内置的 `coder` profile 可以用于媒体分析，因为它从父 Agent 继承了 `ReadMediaFile` 工具。但如果你想要一个更轻量的、专注于媒体分析但不包含文件编辑工具的 profile，可以自定义一个：

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

此 profile：
- 继承 `agent` 的系统提示词模板
- 使用 `gpt-4o` 作为默认模型（这样主 Agent 无需显式传入 `model` 参数）
- 工具集最小化（不含 `Edit` 和 `Write`）
- 自动出现在 `Agent` 工具的子 Agent 列表中

## 使用流程

配置完成后，从用户视角看整个流程是全自动的：

1. **用户粘贴图片**到会话中（拖放、剪贴板粘贴或文件路径）。
2. 主 Agent 的纯文本模型收到类似如下的占位符：

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

3. LLM 调用 `Agent` 工具：

   ```json
   {
     "subagent_type": "media-reader",
     "prompt": "Read the image at /home/user/.mirri-code/media-originals/abc123.png and describe what you see.",
     "model": "gpt-4o"
   }
   ```

4. 子 Agent 读取文件、分析内容，将文本描述返回给主 Agent。

5. 主 Agent 将分析结果整合到回复中——全程无需离开纯文本会话。

::: tip
如果你在自定义 profile 上设置了 `defaultModel`（如上面 `media-reader` 的示例），LLM 甚至不需要传入 `model` 参数——会自动使用 profile 的默认模型。
:::

## 使用建议

- **成本控制**：每次子 Agent 调用都会消耗多模态模型的 token 配额。对于简单的「这张图片里有什么」问题，一次派发就够了。对于需要迭代视觉分析的场景（如对比多张截图），建议直接将会话切换到多模态模型。
- **隐私**：持久化的媒体文件存储在会话目录的本地路径下。除模型 provider 的推理 API 外，不会上传到任何服务器。具体路径见[数据路径](../configuration/data-locations.md)。
- **降级回退**：如果未配置任何多模态模型，占位符会建议 LLM 告知用户无法处理该媒体，并建议用户切换模型或用文字描述媒体内容。

## 相关文档

- [Agent 与子 Agent](../customization/agents.md) — 自定义 Agent profile、模型选择和子 Agent 调度的完整参考
- [平台与模型](../configuration/providers.md) — 如何在 `config.toml` 中配置 `image_in` 等模型能力
