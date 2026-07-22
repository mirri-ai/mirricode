# LLM-Aware Model Selection — Design Document

## 1. Overview

This feature makes the LLM an informed decision-maker when selecting models for subagent dispatch. It adds a `description` field to model aliases, injects curated model lists into tool descriptions, shows per-agent default models, and validates model aliases with actionable errors.

## 2. Config Schema Change

### New field: `description` on `ModelAliasBaseSchema`

**File**: `packages/agent-core/src/config/schema.ts`

```typescript
const ModelAliasBaseSchema = z.object({
  provider: z.string(),
  model: z.string(),
  maxContextSize: z.number().int().min(1),
  maxOutputSize: z.number().int().min(1).optional(),
  capabilities: z.array(z.string()).optional(),
  displayName: z.string().optional(),
  description: z.string().optional(),  // NEW: LLM-facing capability description
  reasoningKey: z.string().optional(),
  protocol: z.literal('anthropic').optional(),
  adaptiveThinking: z.boolean().optional(),
  supportEfforts: z.array(z.string()).optional(),
  defaultEffort: z.string().optional(),
  betaApi: z.boolean().optional(),
});
```

**TOML usage:**

```toml
[models."sonnet"]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_context_size = 200000
description = "balanced, strong at multi-file coding tasks"

[models."haiku"]
provider = "anthropic"
model = "claude-haiku-3-5-20241022"
max_context_size = 200000
description = "fast, best for simple lookups and quick tasks"

[models."experimental-model"]
# No description → not exposed to LLM, still usable as defaultModel
provider = "openai"
model = "gpt-4o"
max_context_size = 128000
```

**Semantics**: `description` serves a dual purpose — it is both the capability label shown to the LLM AND the signal that this model should be exposed in the "Available models" section. Models without `description` are invisible to the LLM's model selection but remain valid for `defaultModel` configuration.

## 3. `buildSubagentDescriptions()` Enhancement

**File**: `packages/agent-core/src/tools/builtin/collaboration/agent.ts`

Add `Default model: <alias>` line after Tools line for each agent:

```typescript
export function buildSubagentDescriptions(subagents: ResolvedAgentProfile['subagents']): string {
  if (subagents === undefined) return '';
  return Object.entries(subagents)
    .map(([name, subagent]) => {
      const details = [subagent.description, subagent.whenToUse].filter(
        (part): part is string => part !== undefined && part.length > 0,
      );
      const header = details.length === 0 ? `- ${name}` : `- ${name}: ${details.join(' ')}`;
      const lines = [header];
      if (subagent.tools.length > 0) {
        lines.push(`  Tools: ${subagent.tools.join(', ')}`);
      }
      // NEW: show default model so LLM knows when not to override
      const modelLabel = subagent.defaultModel ?? 'inherited';
      lines.push(`  Default model: ${modelLabel}`);
      return lines.join('\n');
    })
    .join('\n');
}
```

**Output example:**

```
- coder: General software engineering agent
  Tools: Read, Edit, Bash, ...
  Default model: sonnet
- explore: Fast codebase exploration
  Tools: Bash, Read, Glob, Grep
  Default model: inherited
```

## 4. Available Models Injection

### 4.1 Model list builder

**File**: `packages/agent-core/src/tools/builtin/collaboration/agent.ts`

New function `buildAvailableModelsSection()`:

```typescript
interface ModelDescriptor {
  alias: string;
  description?: string;
  maxContextSize: number;
}

export function buildAvailableModelsSection(models: readonly ModelDescriptor[]): string {
  const exposed = models.filter(m => m.description && m.description.length > 0);
  if (exposed.length === 0) return '';

  const lines = exposed.map(m => {
    const ctxKb = Math.round(m.maxContextSize / 1024);
    return `- ${m.alias} — ${m.description}, ${ctxKb}k context`;
  });

  return 'Available models (only override when task difficulty clearly differs from the agent purpose):\n' + lines.join('\n');
}
```

### 4.2 AgentTool description getter

**File**: `packages/agent-core/src/tools/builtin/collaboration/agent.ts`

The constructor already accepts a `subagentProvider` callback. Add a `modelProvider` callback:

```typescript
constructor(options?: {
  subagentProvider?: (() => Subagents) | Subagents | undefined;
  modelProvider?: (() => readonly ModelDescriptor[]) | undefined;
}) {
  this._subagentProvider = ...;
  this._modelProvider = options?.modelProvider;
}

get description(): string {
  const subagents = this._subagentProvider();
  const typeLines = buildSubagentDescriptions(subagents);

  const modelLines = this._modelProvider
    ? buildAvailableModelsSection(this._modelProvider())
    : '';

  const sections = [this._baseDescription];
  if (typeLines) sections.push(`Available agent types (pass via subagent_type):\n${typeLines}`);
  if (modelLines) sections.push(modelLines);
  return sections.join('\n\n');
}
```

### 4.3 AgentSwarmTool description getter

Same pattern — inject `modelProvider` and append `buildAvailableModelsSection()` to the description.

### 4.4 Wiring in `agent/tool/index.ts`

```typescript
// In AgentToolManager or wherever AgentTool is constructed:
const agentTool = new AgentTool({
  subagentProvider: () => this.agent.subagentHost?.getAvailableSubagents() ?? {},
  modelProvider: () => this.agent.session?.modelCatalogService?.listModelsSync() ?? [],
});
```

Note: `ModelCatalogService.listModels()` is async. We need either:
- A sync cache that's refreshed on config changes, or
- Pre-fetch models at turn start and pass a static array

**Chosen approach**: Pre-fetch at tool construction time (once per turn). The tool description is read once per turn, so we fetch models when the tool manager initializes and pass a static array. If config changes mid-turn (rare), it takes effect next turn.

### 4.5 ModelDescriptor from config

```typescript
// Convert from MirriConfig models to ModelDescriptor
function toModelDescriptors(config: MirriConfig): ModelDescriptor[] {
  return Object.entries(config.models ?? {}).map(([alias, model]) => ({
    alias,
    description: model.description,
    maxContextSize: model.maxContextSize,
  }));
}
```

## 5. `model` Parameter Description — Dynamic

### Current (static, with dead reference):

```typescript
model: z.string().optional().describe(
  'Model alias to override the subagent default. Choose based on task difficulty: larger-context models for complex multi-file tasks, smaller/faster models for simple lookups. If omitted, the subagent uses the profile default or inherits the parent model. See "Available Models" in this tool description for the list.',
)
```

### New approach:

The `model` parameter's `.describe()` is set at construction time based on whether curated models exist:

```typescript
// When curated models exist:
'Model alias to override the subagent default. Only override when task difficulty clearly differs from the agent purpose. Valid aliases: sonnet, haiku.'

// When no curated models exist:
'Model alias to override the subagent default. Most agents already have a sensible default — leave empty unless you have a specific reason to override.'
```

This requires the schema to be built dynamically. Since `AgentTool` already has a constructor, we build the schema in the constructor:

```typescript
class AgentTool {
  readonly inputSchema;
  constructor(options?: { ...; modelProvider?: () => readonly ModelDescriptor[] }) {
    // ...
    const exposedAliases = this._getExposedAliases();
    this.inputSchema = buildAgentToolSchema(exposedAliases);
  }
}
```

**Important**: The schema must be built once at construction time. If models change between turns, the tool manager recreates tools (this is already the pattern for dynamic subagent descriptions).

## 6. Model Alias Validation

**File**: `packages/agent-core/src/session/subagent-host.ts`

In `configureChild()`, before applying the model alias:

```typescript
if (options?.model) {
  const validAliases = this._getValidModelAliases();
  if (!validAliases.includes(options.model)) {
    const curated = this._getCuratedModelAliases();  // models with description
    const hint = curated.length > 0
      ? ` Valid options: ${curated.join(', ')}.`
      : '';
    const defaultHint = profile.defaultModel
      ? ` This agent's default model is "${profile.defaultModel}".`
      : '';
    throw new Error(
      `Model "${options.model}" is not configured.${hint}${defaultHint} Leave the model parameter empty to use the default.`
    );
  }
}
const modelAlias = options?.model ?? profile.defaultModel ?? parent.config.modelAlias;
```

**Error handling**: The error is caught by `AgentTool.execution()` and returned as a tool result (not thrown to the agent loop). The LLM sees the error and can retry with a valid alias or omit the parameter.

**Wait** — actually, `configureChild` throwing would propagate up through `spawn()`. We need to catch this in `AgentTool.execution()` and return it as a structured tool result.

### Better approach: validate in AgentTool.execution()

```typescript
// In AgentTool.execution(), before calling subagentHost.spawn():
if (args.model) {
  const validAliases = await this._getValidModelAliases();
  if (!validAliases.includes(args.model)) {
    return {
      isError: true,
      content: [{
        type: 'text',
        text: `Model "${args.model}" is not configured. ${this._buildModelHint()}`,
      }],
    };
  }
}
```

This keeps validation at the tool layer, where it can return a clean error result without involving the subagent-host. The subagent-host's `configureChild` continues to use the simple `??` chain since invalid aliases never reach it.

## 7. Data Flow

```
config.toml
  └─ models (record of alias → ModelAlias)
       └─ description (optional, triggers LLM exposure)

At turn start:
  AgentToolManager constructs tools
    ├─ reads MirriConfig.models
    ├─ filters models with non-empty description → ModelDescriptor[]
    ├─ passes to AgentTool constructor as modelProvider
    └─ AgentTool builds:
         ├─ inputSchema (with dynamic model param description)
         └─ description getter (with available models section)

At dispatch time:
  LLM calls Agent tool with model parameter
    ├─ AgentTool.execution() validates alias against config.models keys
    │   ├─ valid → proceed to subagentHost.spawn()
    │   └─ invalid → return error result with valid options
    └─ subagentHost.configureChild()
         └─ modelAlias = options.model ?? profile.defaultModel ?? parent.modelAlias
```

## 8. REST API / Web UI

No new REST endpoints needed. The model `description` field is part of `config.toml` and is read by the existing config loading mechanism.

The Web UI model manager (if/when built) can display the `description` field, but that's a separate feature — out of scope for this PR.

## 9. Testing Strategy

### Unit tests (TDD):

1. **`buildSubagentDescriptions`** — adds `Default model: <alias>` / `Default model: inherited`
2. **`buildAvailableModelsSection`** — filters by description, formats correctly, returns empty when no curated models
3. **`AgentTool.description` getter** — includes model section when curated models exist, omits when none
4. **`AgentTool.inputSchema`** — `model` parameter description is dynamic
5. **`AgentTool.execution`** — invalid model alias returns error result, valid alias proceeds
6. **Config schema** — `description` field parses correctly, is optional

### Integration tests:

7. End-to-end: config with `description` → tool description contains model list → LLM sees it
8. End-to-end: config without `description` → no model list in tool description

## 10. Files to Modify

| File | Change |
|------|--------|
| `packages/agent-core/src/config/schema.ts` | Add `description: z.string().optional()` to `ModelAliasBaseSchema` |
| `packages/agent-core/src/tools/builtin/collaboration/agent.ts` | `buildSubagentDescriptions` + `buildAvailableModelsSection` + dynamic schema + validation |
| `packages/agent-core/src/tools/builtin/collaboration/agent-swarm.ts` | Description getter + model provider injection |
| `packages/agent-core/src/agent/tool/index.ts` | Wire `modelProvider` to AgentTool/AgentSwarmTool constructors |
| `packages/agent-core/test/tools/builtin-current.test.ts` | New tests for all above |
| `packages/agent-core/test/config/configs.test.ts` | Test `description` field parsing |
