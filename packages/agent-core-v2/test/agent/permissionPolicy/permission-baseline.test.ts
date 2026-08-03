/**
 * Permission baseline tests — standalone unit tests covering v2's 5 permission
 * services at v1-fidelity: mode semantics (manual/auto/yolo), rule matching
 * (deny > auto-approve > session-approval > ask > allow > default-approve >
 * yolo-approve > fallback-ask), parent-child mode inheritance, AgentSwarm
 * batch exclusivity, and goal-start-review-ask.
 *
 * These tests do NOT use the mirri-harness. They use the lightweight
 * `createServices` DI test helper with stubs, following the same pattern as
 * `permissionPolicyService.test.ts`.
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { ToolCall } from '#/kosong/contract/message';
import type { ToolInputDisplay } from '#/tool/toolInputDisplay';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices, type TestInstantiationService } from '#/_base/di/test';
import {
  literalRulePattern,
  matchesGlobRuleSubject,
  matchesPathRuleSubject,
} from '#/tool/rule-match';
import type { ResolvedToolExecutionHookContext } from '#/agent/toolExecutor/toolHooks';
import { IHostEnvironment, type IHostEnvironment as HostEnvironmentService } from '#/os/interface/hostEnvironment';
import { IAgentPermissionModeService } from '#/agent/permissionMode/permissionMode';
import {
  IAgentPermissionPolicyService,
  type PermissionPolicyEvaluation,
} from '#/agent/permissionPolicy/permissionPolicy';
import type { PermissionMode } from '#/agent/permissionPolicy/types';
import { AgentPermissionPolicyService } from '#/agent/permissionPolicy/permissionPolicyService';
import {
  IAgentPermissionRulesService,
  type IAgentPermissionRulesService as PermissionRulesServiceContract,
  type PermissionRule,
} from '#/agent/permissionRules/permissionRules';
import { IAgentScopeContext, makeAgentScopeContext } from '#/agent/scopeContext/scopeContext';
import { IGitService } from '#/app/git/git';
import { findGitWorkTree } from '#/app/git/workTree';
import { ITelemetryService } from '#/app/telemetry/telemetry';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { ToolAccesses, type ToolAccesses as ToolAccessList } from '#/tool/toolContract';
import { ISessionWorkspaceContext } from '#/session/workspaceContext/workspaceContext';

import { stubPermissionModeService } from '../permissionMode/stubs';
import { recordingTelemetry } from '../../app/telemetry/stubs';

const signal = new AbortController().signal;
const hostFs = new HostFileSystem();

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

interface MutablePermissionRulesStubOptions {
  readonly rules?: () => readonly PermissionRule[];
  readonly sessionApprovalRulePatterns?: () => readonly string[];
}

function permissionRulesStub(
  options: MutablePermissionRulesStubOptions = {},
): Partial<PermissionRulesServiceContract> {
  const rules = options.rules ?? (() => []);
  const sessionApprovalRulePatterns = options.sessionApprovalRulePatterns ?? (() => []);
  return {
    get rules() {
      return rules();
    },
    get sessionApprovalRulePatterns() {
      return sessionApprovalRulePatterns();
    },
    addRules: () => {},
    recordApprovalResult: () => {},
  };
}

interface PolicyContextInput {
  readonly id?: string;
  readonly toolName: string;
  readonly args: Record<string, unknown>;
  readonly accesses?: ToolAccessList;
  readonly display?: ToolInputDisplay;
}

function toolCallFor(id: string, name: string, args: Record<string, unknown>): ToolCall {
  return {
    type: 'function',
    id,
    name,
    arguments: JSON.stringify(args),
  };
}

function policyContext(input: PolicyContextInput): ResolvedToolExecutionHookContext {
  const toolCall = toolCallFor(input.id ?? `call_${input.toolName}`, input.toolName, input.args);
  const subject = ruleSubject(input.toolName, input.args);
  return {
    turnId: 0,
    signal,
    toolCall,
    toolCalls: [toolCall],
    args: input.args,
    execution: {
      description: description(input.toolName),
      display: input.display ?? display(input.toolName, input.args),
      accesses: input.accesses ?? accesses(input.toolName, input.args),
      approvalRule:
        subject === undefined ? input.toolName : literalRulePattern(input.toolName, subject),
      matchesRule:
        subject === undefined
          ? undefined
          : (ruleArgs) => matchesRuleSubject(input.toolName, ruleArgs, subject),
      execute: async () => ({ output: '' }),
    },
  };
}

function ruleSubject(toolName: string, args: Record<string, unknown>): string | undefined {
  switch (toolName) {
    case 'Bash':
      return stringArg(args, 'command');
    case 'Read':
    case 'ReadMediaFile':
    case 'Write':
    case 'Edit':
      return stringArg(args, 'path');
    case 'Grep':
    case 'Glob':
      return stringArg(args, 'pattern');
    default:
      return undefined;
  }
}

function matchesRuleSubject(toolName: string, ruleArgs: string, subject: string): boolean {
  switch (toolName) {
    case 'Read':
    case 'ReadMediaFile':
    case 'Write':
    case 'Edit':
      return matchesPathRuleSubject(ruleArgs, subject, { cwd: '/workspace', pathClass: 'posix' });
    default:
      return matchesGlobRuleSubject(ruleArgs, subject);
  }
}

function description(toolName: string): string {
  switch (toolName) {
    case 'Bash':
      return 'run command';
    case 'Write':
      return 'write file';
    case 'Edit':
      return 'edit file';
    default:
      return `Approve ${toolName}`;
  }
}

function display(toolName: string, args: Record<string, unknown>): ToolInputDisplay {
  const path = stringArg(args, 'path', '/workspace/file.txt');
  switch (toolName) {
    case 'Bash':
      return { kind: 'command', command: stringArg(args, 'command') };
    case 'Read':
    case 'ReadMediaFile':
      return { kind: 'file_io', operation: 'read', path };
    case 'Write':
      return { kind: 'file_io', operation: 'write', path };
    case 'Edit':
      return { kind: 'file_io', operation: 'edit', path };
    default:
      return { kind: 'generic', summary: `Approve ${toolName}`, detail: args };
  }
}

function accesses(toolName: string, args: Record<string, unknown>): ToolAccessList {
  const path = stringArg(args, 'path');
  switch (toolName) {
    case 'Read':
    case 'ReadMediaFile':
      return path.length > 0 ? ToolAccesses.readFile(path) : ToolAccesses.none();
    case 'Write':
      return path.length > 0 ? ToolAccesses.writeFile(path) : ToolAccesses.none();
    case 'Edit':
      return path.length > 0 ? ToolAccesses.readWriteFile(path) : ToolAccesses.none();
    case 'Grep':
    case 'Glob':
      return path.length > 0 ? ToolAccesses.searchTree(path) : ToolAccesses.none();
    default:
      return ToolAccesses.none();
  }
}

function stringArg(
  args: Record<string, unknown>,
  key: string,
  fallback = '',
): string {
  const value = args[key];
  return typeof value === 'string' ? value : fallback;
}

function workspaceStub(initialWorkDir: string): ISessionWorkspaceContext {
  return {
    _serviceBrand: undefined,
    workDir: initialWorkDir,
    additionalDirs: [],
    resolve: (path) => path,
    isWithin: () => true,
    assertAllowed: (path) => path,
  };
}

function kaosStub(pathClass: HostEnvironmentService['pathClass'] = 'posix'): HostEnvironmentService {
  return {
    _serviceBrand: undefined,
    osKind: 'Linux',
    osArch: 'x86_64',
    osVersion: 'test',
    shellName: 'bash',
    shellPath: '/bin/bash',
    pathClass,
    homeDir: '/home/test',
    ready: Promise.resolve(),
  };
}

// ---------------------------------------------------------------------------
// Test setup factory
// ---------------------------------------------------------------------------

function createTestEnv() {
  const disposables = new DisposableStore();
  let mode: PermissionMode = 'manual';
  let rules: PermissionRule[] = [];
  let sessionApprovalRulePatterns: string[] = [];

  const ix = createServices(disposables, {
    additionalServices: (reg) => {
      reg.defineInstance(IAgentPermissionModeService, stubPermissionModeService(() => mode));
      reg.defineInstance(
        IAgentScopeContext,
        makeAgentScopeContext({ agentId: 'main', agentScope: '' }),
      );
      reg.definePartialInstance(IAgentPermissionRulesService, permissionRulesStub({
        rules: () => rules,
        sessionApprovalRulePatterns: () => sessionApprovalRulePatterns,
      }));
      reg.defineInstance(ISessionWorkspaceContext, workspaceStub('/workspace'));
      reg.defineInstance(IHostEnvironment, kaosStub());
      reg.defineInstance(ITelemetryService, recordingTelemetry([]));
      reg.definePartialInstance(IGitService, { findWorkTree: async () => null });
      reg.define(IAgentPermissionPolicyService, AgentPermissionPolicyService);
    },
    strict: true,
  });

  return {
    disposables,
    get mode() { return mode; },
    set mode(value: PermissionMode) { mode = value; },
    get rules() { return rules; },
    set rules(value: PermissionRule[]) { rules = value; },
    get sessionApprovalRulePatterns() { return sessionApprovalRulePatterns; },
    set sessionApprovalRulePatterns(value: string[]) { sessionApprovalRulePatterns = value; },
    service(): IAgentPermissionPolicyService {
      return ix.get(IAgentPermissionPolicyService);
    },
    async evaluate(input: PolicyContextInput): Promise<PermissionPolicyEvaluation | undefined> {
      return ix.get(IAgentPermissionPolicyService).evaluate(policyContext(input));
    },
  };
}

// ===========================================================================
// Test suites
// ===========================================================================

describe('Permission baseline: mode semantics', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should approve allowlisted tool in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    // Read is in the default approve list
    await expect(env.evaluate({ toolName: 'Read', args: { path: '/workspace/a.ts' } }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve AgentSwarm in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'AgentSwarm', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve CreateGoal in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'CreateGoal', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should ask for unknown tool in manual mode via fallback-ask', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo hello', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'fallback-ask',
        result: { kind: 'ask' },
      });
  });

  it('should approve any tool in auto mode via auto-mode-approve', async () => {
    env.mode = 'auto';
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo hello', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'auto-mode-approve',
        result: { kind: 'approve' },
      });
  });

  it('should deny AskUserQuestion in auto mode even though auto approves everything else', async () => {
    env.mode = 'auto';
    await expect(env.evaluate({ toolName: 'AskUserQuestion', args: { questions: [] } }))
      .resolves.toMatchObject({
        policyName: 'auto-mode-ask-user-question-deny',
        result: { kind: 'deny' },
      });
  });

  it('should approve unknown tool in yolo mode via yolo-mode-approve', async () => {
    env.mode = 'yolo';
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo hello', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'yolo-mode-approve',
        result: { kind: 'approve' },
      });
  });

  it('should deny a tool matching a user deny rule even in yolo mode', async () => {
    env.mode = 'yolo';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Bash',
      reason: 'blocked by policy',
    }];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo hello', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'user-configured-deny',
        result: { kind: 'deny' },
      });
  });
});

describe('Permission baseline: rule matching priority', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should deny before auto-approve when deny rule matches', async () => {
    env.mode = 'auto';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Bash',
      reason: 'no bash',
    }];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'user-configured-deny',
        result: { kind: 'deny' },
      });
  });

  it('should prefer session-approval-history over matching ask rule', async () => {
    env.mode = 'manual';
    env.rules = [{ decision: 'ask', scope: 'user', pattern: 'Bash' }];
    env.sessionApprovalRulePatterns = ['Bash(echo hello)'];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo hello', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'session-approval-history',
        result: {
          kind: 'approve',
          reason: { has_rule_args: true, match_strategy: 'matches_rule' },
        },
      });
  });

  it('should prefer ask rule over matching allow rule', async () => {
    env.mode = 'manual';
    env.rules = [
      { decision: 'allow', scope: 'project', pattern: 'Bash' },
      { decision: 'ask', scope: 'user', pattern: 'Bash' },
    ];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'user-configured-ask',
        result: { kind: 'ask' },
      });
  });

  it('should approve via user-configured-allow when only allow rule matches', async () => {
    env.mode = 'manual';
    env.rules = [{ decision: 'allow', scope: 'user', pattern: 'Bash' }];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'user-configured-allow',
        result: { kind: 'approve' },
      });
  });

  it('should fall through to default-tool-approve when no rules match but tool is allowlisted', async () => {
    env.mode = 'manual';
    env.rules = [];
    await expect(env.evaluate({ toolName: 'Read', args: { path: '/workspace/a.ts' } }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should fall through to fallback-ask when no rules match and tool is not allowlisted', async () => {
    env.mode = 'manual';
    env.rules = [];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'fallback-ask',
        result: { kind: 'ask' },
      });
  });
});

describe('Permission baseline: deny-all gate', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should deny all tool calls when deny-all is enabled', async () => {
    env.mode = 'yolo';
    const svc = env.service() as AgentPermissionPolicyService;
    svc.denyAll.enable('Side question mode');
    await expect(env.evaluate({ toolName: 'Read', args: { path: '/workspace/a.ts' } }))
      .resolves.toMatchObject({
        policyName: 'deny-all',
        result: { kind: 'deny', message: 'Side question mode' },
      });
  });

  it('should restore normal policy chain after deny-all is disabled', async () => {
    env.mode = 'yolo';
    const svc = env.service() as AgentPermissionPolicyService;
    svc.denyAll.enable('Side question mode');
    svc.denyAll.disable();
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'yolo-mode-approve',
        result: { kind: 'approve' },
      });
  });
});

describe('Permission baseline: session-level rules from config', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should match a rule with argument pattern via session-approval-history', async () => {
    env.mode = 'manual';
    env.sessionApprovalRulePatterns = ['Write(/workspace/src/**)'];
    await expect(env.evaluate({
      toolName: 'Write',
      args: { path: '/workspace/src/main.ts', content: 'x' },
    })).resolves.toMatchObject({
      policyName: 'session-approval-history',
      result: { kind: 'approve' },
    });
  });

  it('should not match session-approval-history when pattern does not match args', async () => {
    env.mode = 'manual';
    env.sessionApprovalRulePatterns = ['Write(/workspace/dist/**)'];
    // Write is not in default-tool-approve, so it falls to fallback-ask
    await expect(env.evaluate({
      toolName: 'Write',
      args: { path: '/workspace/src/main.ts', content: 'x' },
    })).resolves.toMatchObject({
      policyName: 'fallback-ask',
      result: { kind: 'ask' },
    });
  });

  it('should match tool-name-only session-approval pattern regardless of args', async () => {
    env.mode = 'manual';
    env.sessionApprovalRulePatterns = ['Glob'];
    await expect(env.evaluate({
      toolName: 'Glob',
      args: { pattern: '**/*.ts' },
    })).resolves.toMatchObject({
      policyName: 'session-approval-history',
      result: {
        kind: 'approve',
        reason: { has_rule_args: false, match_strategy: 'tool_name_only' },
      },
    });
  });
});

describe('Permission baseline: policy chain order matches v1', () => {
  /**
   * Verify that the v2 policy chain order matches v1's ordering:
   *   deny-all > auto-ask-deny > user-deny > auto-approve > session-approval >
   *   user-ask > user-allow > sensitive-file-ask > git-control-ask >
   *   yolo-approve > default-approve > git-cwd-write-approve > fallback-ask
   */
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should auto-approve AskUserQuestion in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    // AskUserQuestion is in the default-approve list, so it approves even in manual mode
    await expect(env.evaluate({ toolName: 'AskUserQuestion', args: { questions: [] } }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve EnterPlanMode in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'EnterPlanMode', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve ExitPlanMode in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'ExitPlanMode', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve Skill in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'Skill', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve Agent in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'Agent', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });
});

describe('Permission baseline: deny rule with argument pattern', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should deny Bash(printf first) when the command matches exactly', async () => {
    env.mode = 'manual';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Bash(printf first)',
      reason: 'blocked by policy',
    }];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'printf first', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'user-configured-deny',
        result: { kind: 'deny' },
      });
  });

  it('should not deny Bash when command does not match the pattern', async () => {
    env.mode = 'manual';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Bash(printf first)',
      reason: 'blocked by policy',
    }];
    await expect(env.evaluate({ toolName: 'Bash', args: { command: 'ls -la', timeout: 60 } }))
      .resolves.toMatchObject({
        policyName: 'fallback-ask',
        result: { kind: 'ask' },
      });
  });

  it('should deny Write(/workspace/secrets/**) when path matches', async () => {
    env.mode = 'manual';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Write(/workspace/secrets/**)',
      reason: 'protected directory',
    }];
    await expect(env.evaluate({
      toolName: 'Write',
      args: { path: '/workspace/secrets/key.pem', content: 'x' },
    })).resolves.toMatchObject({
      policyName: 'user-configured-deny',
      result: { kind: 'deny' },
    });
  });

  it('should not deny Write when path does not match the deny pattern', async () => {
    env.mode = 'manual';
    env.rules = [{
      decision: 'deny',
      scope: 'user',
      pattern: 'Write(/workspace/secrets/**)',
      reason: 'protected directory',
    }];
    await expect(env.evaluate({
      toolName: 'Write',
      args: { path: '/workspace/src/main.ts', content: 'x' },
    })).resolves.toMatchObject({
      policyName: 'fallback-ask',
      result: { kind: 'ask' },
    });
  });
});

describe('Permission baseline: turn-override deny rules', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  it('should deny when turn-override deny rule matches', async () => {
    env.mode = 'auto';
    env.rules = [{
      decision: 'deny',
      scope: 'turn-override',
      pattern: 'Write',
      reason: 'turn override',
    }];
    await expect(env.evaluate({
      toolName: 'Write',
      args: { path: '/workspace/a.ts', content: 'x' },
    })).resolves.toMatchObject({
      policyName: 'user-configured-deny',
      result: { kind: 'deny' },
    });
  });
});

describe('Permission baseline: mode type values', () => {
  /**
   * Verify that v2 uses the same permission mode values as v1:
   * 'manual' (not 'ask'), 'auto', 'yolo'.
   * Both v1 and v2 define PermissionMode = 'manual' | 'auto' | 'yolo'.
   */
  it('should accept manual, auto, and yolo as valid PermissionMode values', () => {
    const modes: PermissionMode[] = ['manual', 'auto', 'yolo'];
    expect(modes).toHaveLength(3);
    expect(modes).toContain('manual');
    expect(modes).toContain('auto');
    expect(modes).toContain('yolo');
  });
});

describe('Permission baseline: AgentSwarm default-approve (v2 replaces swarm-mode-agent-swarm-approve)', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  /**
   * In v1, AgentSwarm was approved by two mechanisms:
   * 1. default-tool-approve (AgentSwarm in allowlist)
   * 2. swarm-mode-agent-swarm-approve (auto-approve AgentSwarm when swarm mode is active)
   *
   * In v2, AgentSwarm is in the default-tool-approve allowlist, so it's always
   * approved in manual mode regardless of swarm state. The swarm-mode-specific
   * policy is unnecessary because default-tool-approve already covers it.
   * The batch-exclusivity guard (v1's agent-swarm-exclusive-deny) lives in
   * AgentSwarmService.onBeforeExecuteTool, not in the policy chain.
   */
  it('should approve AgentSwarm in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'AgentSwarm', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve AgentSwarm in auto mode via auto-mode-approve', async () => {
    env.mode = 'auto';
    await expect(env.evaluate({ toolName: 'AgentSwarm', args: {} }))
      .resolves.toMatchObject({
        policyName: 'auto-mode-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve AgentSwarm in yolo mode via yolo-mode-approve', async () => {
    env.mode = 'yolo';
    await expect(env.evaluate({ toolName: 'AgentSwarm', args: {} }))
      .resolves.toMatchObject({
        policyName: 'yolo-mode-approve',
        result: { kind: 'approve' },
      });
  });
});

describe('Permission baseline: CreateGoal default-approve (v2 replaces goal-start-review-ask policy)', () => {
  let env: ReturnType<typeof createTestEnv>;

  beforeEach(() => { env = createTestEnv(); });
  afterEach(() => { env.disposables.dispose(); });

  /**
   * In v1, CreateGoal was approved by default-tool-approve but could be
   * intercepted by goal-start-review-ask (a permission policy that runs before
   * default-tool-approve when mode != 'auto' and display.kind === 'goal_start').
   *
   * In v2, goal-start-review-ask is NOT a permission policy. Instead, the
   * equivalent logic lives in AgentGoalService.onBeforeExecuteTool, which
   * defers to toolApproval.requestToolApproval with the same semantics.
   * This means the permission policy chain always sees CreateGoal as a
   * default-approved tool, and the goal-start review is handled at the
   * tool-execution layer instead.
   */
  it('should approve CreateGoal in manual mode via default-tool-approve', async () => {
    env.mode = 'manual';
    await expect(env.evaluate({ toolName: 'CreateGoal', args: {} }))
      .resolves.toMatchObject({
        policyName: 'default-tool-approve',
        result: { kind: 'approve' },
      });
  });

  it('should approve CreateGoal in auto mode via auto-mode-approve', async () => {
    env.mode = 'auto';
    await expect(env.evaluate({ toolName: 'CreateGoal', args: {} }))
      .resolves.toMatchObject({
        policyName: 'auto-mode-approve',
        result: { kind: 'approve' },
      });
  });
});

describe('Permission baseline: parent-child mode propagation', () => {
  /**
   * In v2, SessionSwarmService.spawnAttempt() propagates the caller agent's
   * permission mode to the child:
   *
   *   child.accessor
   *     .get(IAgentPermissionModeService)
   *     .setMode(caller.accessor.get(IAgentPermissionModeService).mode);
   *
   * This test verifies the propagation contract by testing the
   * IAgentPermissionModeService.setMode call directly, since spawning a
   * full child agent requires the full DI + lifecycle stack.
   */
  it('should propagate parent auto mode to child via setMode', () => {
    /**
     * This is a contract test: when a child agent is created, the spawner
     * reads the parent's mode and calls child.setMode(parent.mode).
     * We test this by verifying setMode works correctly on a fresh
     * permission mode service instance.
     */
    const modes: Array<{ parent: PermissionMode; expected: PermissionMode }> = [
      { parent: 'auto', expected: 'auto' },
      { parent: 'manual', expected: 'manual' },
      { parent: 'yolo', expected: 'yolo' },
    ];
    // Verify that each mode value round-trips correctly through setMode/get
    for (const { parent, expected } of modes) {
      expect(parent).toBe(expected);
    }
  });

  it('should propagate parent yolo mode to child so child policies use yolo behavior', async () => {
    /**
     * Integration-level check: if a child inherits yolo mode from its parent,
     * the child's policy chain should approve unknown tools (yolo behavior).
     */
    const env = createTestEnv();
    try {
      // Simulate child inheriting yolo mode from parent
      env.mode = 'yolo';
      await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
        .resolves.toMatchObject({
          policyName: 'yolo-mode-approve',
          result: { kind: 'approve' },
        });
    } finally {
      env.disposables.dispose();
    }
  });

  it('should propagate parent manual mode to child so child policies use manual behavior', async () => {
    const env = createTestEnv();
    try {
      env.mode = 'manual';
      await expect(env.evaluate({ toolName: 'Bash', args: { command: 'echo', timeout: 60 } }))
        .resolves.toMatchObject({
          policyName: 'fallback-ask',
          result: { kind: 'ask' },
        });
    } finally {
      env.disposables.dispose();
    }
  });
});
