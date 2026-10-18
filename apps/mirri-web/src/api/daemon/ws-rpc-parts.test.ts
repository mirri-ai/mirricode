// apps/mirri-web/src/api/daemon/ws-rpc-parts.test.ts
// Unit tests for the WsRpcApi prompt-content → rust wire part mapping (P4):
// the v1 TS REST content spellings the rust `message.send` entry accepts.

import { describe, expect, it } from 'vitest';

import {
  fetchWorkspaceSkills,
  pendingApprovalEventsFromSnapshot,
  promptContentToInputParts,
  toAppPendingApproval,
} from './ws-rpc-api';
import type { AppMessageContent } from '../types';

describe('promptContentToInputParts (rust message.send input wire)', () => {
  it('maps text parts when the prompt is plain', () => {
    const content: AppMessageContent[] = [{ type: 'text', text: 'hello' }];

    expect(promptContentToInputParts(content)).toEqual([{ type: 'text', text: 'hello' }]);
  });

  it('maps image parts across the three source kinds', () => {
    const content: AppMessageContent[] = [
      { type: 'image', source: { kind: 'base64', mediaType: 'image/png', data: 'aGk=' } },
      { type: 'image', source: { kind: 'file', fileId: 'f_img123' } },
      { type: 'image', source: { kind: 'url', url: 'https://example.com/cat.png' } },
    ];

    expect(promptContentToInputParts(content)).toEqual([
      { type: 'image', source: { kind: 'base64', media_type: 'image/png', data: 'aGk=' } },
      { type: 'image', source: { kind: 'file', file_id: 'f_img123' } },
      { type: 'image', source: { kind: 'url', url: 'https://example.com/cat.png' } },
    ]);
  });

  it('maps video parts across the three source kinds', () => {
    const content: AppMessageContent[] = [
      { type: 'video', source: { kind: 'base64', mediaType: 'video/mp4', data: 'dnM=' } },
      { type: 'video', source: { kind: 'file', fileId: 'f_vid456' } },
      { type: 'video', source: { kind: 'url', url: 'https://example.com/clip.mp4' } },
    ];

    expect(promptContentToInputParts(content)).toEqual([
      { type: 'video', source: { kind: 'base64', media_type: 'video/mp4', data: 'dnM=' } },
      { type: 'video', source: { kind: 'file', file_id: 'f_vid456' } },
      { type: 'video', source: { kind: 'url', url: 'https://example.com/clip.mp4' } },
    ]);
  });

  it('maps file parts carrying the upload metadata', () => {
    const content: AppMessageContent[] = [
      {
        type: 'file',
        fileId: 'f_doc789',
        name: 'report.pdf',
        mediaType: 'application/pdf',
        size: 1024,
      },
    ];

    expect(promptContentToInputParts(content)).toEqual([
      {
        type: 'file',
        file_id: 'f_doc789',
        name: 'report.pdf',
        media_type: 'application/pdf',
        size: 1024,
      },
    ]);
  });

  it('maps mixed content and drops parts without a prompt wire form', () => {
    const content: AppMessageContent[] = [
      { type: 'text', text: 'see the attachment' },
      { type: 'image', source: { kind: 'file', fileId: 'f_img123' } },
      {
        type: 'toolUse',
        toolCallId: 'call_1',
        toolName: 'Bash',
        input: {},
      },
      {
        type: 'file',
        fileId: 'f_doc789',
        name: 'report.pdf',
        mediaType: 'application/pdf',
        size: 9,
      },
    ];

    expect(promptContentToInputParts(content)).toEqual([
      { type: 'text', text: 'see the attachment' },
      { type: 'image', source: { kind: 'file', file_id: 'f_img123' } },
      {
        type: 'file',
        file_id: 'f_doc789',
        name: 'report.pdf',
        media_type: 'application/pdf',
        size: 9,
      },
    ]);
  });

  it('answers an empty array when no part has a wire form', () => {
    const content: AppMessageContent[] = [
      { type: 'thinking', thinking: 'hmm' },
      { type: 'unknown', raw: {} },
    ];

    expect(promptContentToInputParts(content)).toEqual([]);
  });
});

describe('fetchWorkspaceSkills (rust skill.list management fields)', () => {
  it('passes through path and enabled onto AppSkill when the wire rows carry them', async () => {
    const skills = await fetchWorkspaceSkills(async () => [
      { name: 'pdf', description: 'PDFs', source: 'extra', path: '/x/pdf', enabled: false },
      { name: 'greet', description: '', source: 'project', path: '/y/greet' },
    ], 'ws-1');

    expect(skills).toEqual([
      { name: 'pdf', description: 'PDFs', source: 'extra', path: '/x/pdf', enabled: false },
      // enabled absent (v2 backend) = not disabled; do not fabricate a false
      { name: 'greet', description: '', source: 'project', path: '/y/greet' },
    ]);
  });
});

describe('fetchWorkspaceSkills (rust skill.list workspace view)', () => {
  it('reads skill.list and maps the wire rows onto AppSkill', async () => {
    const calls: Array<{ method: string; params: unknown }> = [];
    const skills = await fetchWorkspaceSkills(async (method, params) => {
      calls.push({ method, params });
      return [
        { name: 'code-review', description: 'Review code carefully', source: 'user' },
        { name: 'pdf', description: '', source: 'extra' },
      ];
    }, 'ws-1');

    // Then the workspace view rode the same skill.list route the session
    // view uses, addressed to the workspace
    expect(calls).toEqual([{ method: 'skill.list', params: { workspaceId: 'ws-1' } }]);
    expect(skills).toEqual([
      { name: 'code-review', description: 'Review code carefully', source: 'user' },
      { name: 'pdf', description: '', source: 'extra' },
    ]);
  });

  it('maps missing optional fields onto honest defaults', async () => {
    const skills = await fetchWorkspaceSkills(
      async () => [{ name: 'plain' }] as unknown[],
      'ws-1',
    );

    expect(skills).toEqual([{ name: 'plain', description: '', source: 'builtin' }]);
  });

  it('answers an empty list when the engine returns a non-array', async () => {
    const skills = await fetchWorkspaceSkills(async () => null, 'ws-1');

    expect(skills).toEqual([]);
  });
});

describe('pending-approval resync (ADR-011 e: reconnect rebuild from the gate read face)', () => {
  const row = {
    approval_id: '8f3c9a10-0000-4000-8000-000000000001',
    session_id: 's1',
    turn_id: 3,
    tool_call_id: 'call_resynced',
    tool_name: 'Bash',
    action: 'execute Bash',
    tool_input_display: { summary: 'echo hi' },
    approvalRule: 'Bash',
    agent_id: 'agent-abc',
    created_at: '2026-01-01T00:00:00.000Z',
  } as const;

  it('maps a snapshot pending-approval row onto the ask-shape approval request', () => {
    const approval = toAppPendingApproval(row, 's1');
    expect(approval).toEqual({
      approvalId: row.approval_id,
      sessionId: 's1',
      turnId: 3,
      toolCallId: 'call_resynced',
      toolName: 'Bash',
      action: 'execute Bash',
      display: { summary: 'echo hi' },
      approvalRule: 'Bash',
      agentId: 'agent-abc',
      createdAt: row.created_at,
    });
  });

  it('builds one approvalRequested event per pending row and none when the list is empty', () => {
    const events = pendingApprovalEventsFromSnapshot({ pending_approvals: [row] }, 's1');
    expect(events).toHaveLength(1);
    const first = events[0]!;
    expect(first.type).toBe('approvalRequested');
    if (first.type !== 'approvalRequested') throw new Error('expected an approvalRequested event');
    expect(first.sessionId).toBe('s1');
    expect(first.approval.approvalId).toBe(row.approval_id);

    expect(pendingApprovalEventsFromSnapshot({}, 's1')).toEqual([]);
    expect(pendingApprovalEventsFromSnapshot({ pending_approvals: [] }, 's1')).toEqual([]);
  });
});
