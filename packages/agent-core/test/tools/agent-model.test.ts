import { describe, expect, it } from 'vitest';

import { AgentToolInputSchema } from '../../src/tools/builtin/collaboration/agent';
import { AgentSwarmToolInputSchema } from '../../src/tools/builtin/collaboration/agent-swarm';

describe('Agent tool per-subagent model override', () => {
  it('should accept model parameter in AgentToolInput', () => {
    const input = AgentToolInputSchema.parse({
      prompt: 'test task',
      description: 'test task',
      subagent_type: 'coder',
      model: 'gpt-4o',
    });
    expect(input.model).toBe('gpt-4o');
  });

  it('should default model to undefined when not provided', () => {
    const input = AgentToolInputSchema.parse({
      prompt: 'test',
      description: 'test',
    });
    expect(input.model).toBeUndefined();
  });

  it('should accept model in AgentSwarmToolInput', () => {
    const input = AgentSwarmToolInputSchema.parse({
      description: 'swarm task',
      items: ['a', 'b'],
      prompt_template: 'do {{item}}',
      model: 'claude-haiku',
    });
    expect(input.model).toBe('claude-haiku');
  });

  it('should default swarm model to undefined when not provided', () => {
    const input = AgentSwarmToolInputSchema.parse({
      description: 'swarm',
      items: ['a', 'b'],
      prompt_template: 'do {{item}}',
    });
    expect(input.model).toBeUndefined();
  });
});
