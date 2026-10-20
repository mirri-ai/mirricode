# LLM-aware model selection for sub-agents

Which model should a dispatched sub-agent use? Most tools answer "you decide" — you pick a default in settings and every sub-agent runs on it, one speed and one price for every task. But tasks come in different weights, and a single gear is a mismatch by definition.

Mirri Code hands this decision to **the party that knows the task best**: the LLM that is decomposing it.

## What it is

When the main agent dispatches sub-agents through the `Agent` / `AgentSwarm` tools, the `model` parameter is not hardcoded in a config file — the LLM picks it **on the spot**, based on the nature of each subtask:

- The main agent splits out "find where this function is defined across the repo" — it knows this is lightweight exploration and dispatches a cheap, fast model.
- It splits out "refactor this module's concurrency-safety logic" — it knows this needs deep reasoning and dispatches a flagship model.

The key is that the LLM decides with full information: each sub-agent profile's tool description carries its `defaultModel` and capabilities, and models with a `description` in `config.toml` are listed as selectable candidates along with their strengths. It isn't guessing — it's choosing from a labeled shelf.

Model resolution follows a three-tier priority:

1. **Explicit LLM choice** — the `model` parameter passed in the tool call
2. **Profile default** — the `defaultModel` declared on the sub-agent profile
3. **Inherit from parent** — the model alias the main agent is currently using

If the LLM passes an invalid model alias, the tool returns an error listing the valid options and it self-corrects on the next turn — no human intervention needed.

## Why it deserves its own page

Compare the mainstream approaches:

| Approach | Who decides the model | Problem |
|----------|----------------------|---------|
| One fixed global default | You (set once) | Heavy tasks underpowered, light tasks overpriced |
| Manual binding per sub-agent type | You (configure each) | Large configuration surface, and a pinned gear never adapts to the task |
| **Mirri Code: LLM picks per task** | The LLM (at every dispatch) | The decision lands exactly where the task is already decomposed and its nature is known |

The essence of the design: **the moment a task is decomposed is precisely the moment model-selection information is at its richest**. The main agent knows better than any static config whether a subtask is "look it up" or "think it through" — letting it finish the selection right there beats you pre-enumerating every combination.

## The decisions you get to make

With model selection handed to the LLM, you're still left holding three decisions — and only these three:

**Decision one: what goes on the shelf.** Give any model you want in the candidate pool a `description` in `config.toml` — that line of text is the label the LLM reads when picking:

```toml
[models."glm-5.3"]
provider = "zhipu"
model = "glm-5.3"
max_context_size = 200000
description = "strong reasoning, best for complex coding and refactoring"

[models."mimo-flash"]
provider = "mimo"
model = "MiMo-V2.6-Flash"
max_context_size = 128000
description = "cheap and fast, best for simple lookups and quick tasks"
capabilities = ["image_in", "tool_use"]
```

The more your `description` reads like "what this model should be used for", the better the LLM chooses. Conversely, models without a `description` never appear in the LLM-visible list — that's deliberate: the shelf only stocks what you curated, keeping the choice surface clean.

**Decision two: which routes get a default.** For sub-agent types where "this kind of work almost always uses this model", declare a `defaultModel` in the profile and spare the LLM the live selection:

```yaml
# ~/.mirri-code/agents/media-reader.yaml
defaultModel: mimo-flash
```

For example, media analysis almost always runs on a cheap multimodal model — that route usually has no suspense, so give it a default and the LLM rarely needs to think about it. Note that it's a **default**, not a lock: when the LLM judges there's a better fit, it can still override it via the `model` parameter.

**Decision three: do nothing.** Also a perfectly valid option. Skip both decisions and sub-agents fall back through "LLM picks live → profile default → inherit the main agent" — plenty for everyday use. The shelf can be stocked later.

## How it pairs with multimodal dispatch

This mechanism and [multimodal access via sub-agent](./multimodal-via-subagent.md) are two layers of guidance in one dispatch system: the main agent first picks **who** to dispatch (subagent → sub-agent profile), then **which brain** to run it on (model → model alias). The "text-only workhorse with a multimodal sidekick" story in the multimodal doc is a textbook case of both layers in action — upon seeing the placeholder, the LLM picks the cheap vision model with `image_in` capability off the shelf, and a single dispatch accomplishes what the main model can't do: seeing the image.

Conversely, automatic model selection makes multimodal setup easier too: you don't need to maintain a dedicated dispatch route for "looking at images" — as long as the vision model is on the shelf with a clear `description`, the LLM will naturally reach for it at the moment vision is needed.

## Related

- [Multimodal access via sub-agent](./multimodal-via-subagent.md) — the flagship application of two-layer guidance: a multimodal sidekick for a text-only main model
- [Agents and Subagents](../customization/agents.md) — full reference for profile fields, `defaultModel` priority, and model resolution
- [Providers and Models](../configuration/providers.md) — declaring models, `description`, and `capabilities` in `config.toml`
