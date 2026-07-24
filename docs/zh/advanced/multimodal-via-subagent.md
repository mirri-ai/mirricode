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
name: media-reader
extends: agent
description: 多模态分析专家，用于图片和视频
defaultModel: gpt-4o
whenToUse: 需要分析图片、截图或视频文件时使用此 Agent
tools:
  - ReadMediaFile
  - Read
  - Bash
promptVars:
  roleAdditional: |
    你是媒体分析专家。你的主要工作是使用 ReadMediaFile 工具读取和描述
    图片、视频和音频文件。清晰简洁地报告你看到的内容。
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
