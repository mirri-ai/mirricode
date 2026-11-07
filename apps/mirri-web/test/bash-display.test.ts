import { describe, expect, it } from 'vitest';
import {
  bashCommandOf,
  bashExtraFieldsOf,
  bashPrefixText,
  clipBashSummary,
  summarizeBashCommand,
} from '../src/lib/bashDisplay';

const WS = '/Users/me/workspace/proj';

describe('summarizeBashCommand', () => {
  it('should show a workspace badge when the leading cd targets the session workspace', () => {
    // Given a command that cd's into the session workspace first
    // When the row summary is built
    // Then the path collapses to a workspace badge and the command leads
    const d = summarizeBashCommand(`cd ${WS} && pnpm test`, WS);
    expect(d.prefix).toEqual({ kind: 'workspace', label: 'proj', path: WS });
    expect(d.text).toBe('pnpm test');
  });

  it('should treat a trailing slash on the cd path as the same workspace', () => {
    const d = summarizeBashCommand(`cd ${WS}/ && pnpm test`, WS);
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('pnpm test');
  });

  it('should treat cd . as the session workspace', () => {
    const d = summarizeBashCommand('cd . && pnpm test', WS);
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('pnpm test');
  });

  it('should compress a cd to another directory instead of badging it as the workspace', () => {
    // Given a cd into a directory that is NOT the session workspace
    // When the row summary is built
    // Then it compresses to `…/dir/ &&` — never the workspace badge
    const d = summarizeBashCommand('cd /tmp/build-output && make all', WS);
    expect(d.prefix).toEqual({ kind: 'other', label: 'build-output', path: '/tmp/build-output' });
    expect(bashPrefixText(d.prefix!)).toBe('…/build-output/ && ');
    expect(d.text).toBe('make all');
  });

  it('should not badge a workspace subdirectory as the workspace', () => {
    const d = summarizeBashCommand(`cd ${WS}/packages/web && pnpm dev`, WS);
    expect(d.prefix?.kind).toBe('other');
    expect(d.prefix?.label).toBe('web');
  });

  it('should keep a relative cd path verbatim because it cannot be proven to be the workspace', () => {
    // Given a relative directory change
    // When the row summary is built
    // Then nothing is folded away: compressing it could misreport the cwd
    const d = summarizeBashCommand('cd ../sibling && pnpm test', WS);
    expect(d.prefix).toBeUndefined();
    expect(d.text).toBe('cd ../sibling && pnpm test');
  });

  it('should keep a cd that is not at the head of the command', () => {
    const d = summarizeBashCommand('pnpm build && cd /tmp && ls', WS);
    expect(d.prefix).toBeUndefined();
    expect(d.text).toBe('pnpm build && cd /tmp && ls');
  });

  it('should keep a subshell cd', () => {
    const d = summarizeBashCommand('(cd /tmp && ls)', WS);
    expect(d.prefix).toBeUndefined();
  });

  it('should handle a quoted cd path that contains spaces', () => {
    const d = summarizeBashCommand('cd "/Users/me/my project" && pnpm test', '/Users/me/my project');
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('pnpm test');
  });

  it('should accept a ~-relative cd path when it identifies the workspace', () => {
    const d = summarizeBashCommand('cd ~/workspace/proj && pnpm test', '/Users/me/workspace/proj');
    expect(d.prefix?.kind).toBe('workspace');
  });

  it('should not accept a ~-relative path when it identifies a different directory', () => {
    const d = summarizeBashCommand('cd ~/workspace/other && pnpm test', '/Users/me/workspace/proj');
    expect(d.prefix?.kind).toBe('other');
  });

  it('should not badge anything when the workspace root is unknown', () => {
    // Given no workspace root
    // When the command cd's to an absolute path
    // Then it compresses but is never presented as the workspace
    const d = summarizeBashCommand(`cd ${WS} && pnpm test`, undefined);
    expect(d.prefix?.kind).toBe('other');
  });

  it('should keep the command verbatim in full mode', () => {
    const command = `cd ${WS} && pnpm test`;
    const d = summarizeBashCommand(command, WS, { full: true });
    expect(d.prefix).toBeUndefined();
    expect(d.text).toBe(command);
  });

  it('should keep a bare cd with no following command', () => {
    const d = summarizeBashCommand(`cd ${WS}`, WS);
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('');
  });

  it('should flatten a multi-line command onto one row', () => {
    const d = summarizeBashCommand('pnpm test\n  --reporter verbose', WS);
    expect(d.text).toBe('pnpm test --reporter verbose');
  });
});

describe('clipBashSummary', () => {
  it('should keep a short command unchanged', () => {
    expect(clipBashSummary('pnpm test')).toBe('pnpm test');
  });

  it('should keep both ends when the command is longer than the budget', () => {
    // Given a command far longer than the summary budget
    // When it is clipped
    // Then the head (verb) and the tail (the distinguishing argument) both survive
    const out = clipBashSummary(`rtk grep ${'-'.repeat(400)} --include=cli`, 60);
    expect(out.startsWith('rtk grep')).toBe(true);
    expect(out.endsWith('--include=cli')).toBe(true);
    expect(out.length).toBeLessThanOrEqual(60);
  });

  it('should never split into more than one visible ellipsis', () => {
    const out = clipBashSummary(`git log ${'a'.repeat(500)}`, 40);
    expect(out.match(/…/g)).toHaveLength(1);
  });
});

describe('bashCommandOf', () => {
  it('should read the command out of a JSON tool argument', () => {
    expect(bashCommandOf(JSON.stringify({ command: 'pnpm test', timeout: 30 }))).toBe('pnpm test');
  });

  it('should accept a plain string argument as the command itself', () => {
    expect(bashCommandOf('pnpm test')).toBe('pnpm test');
  });

  it('should return undefined when the argument carries no command', () => {
    expect(bashCommandOf(JSON.stringify({ cwd: '/tmp' }))).toBeUndefined();
    expect(bashCommandOf('')).toBeUndefined();
  });
});

describe('bashExtraFieldsOf', () => {
  it('should expose the remaining input fields without the command wrapper', () => {
    // Given a Bash argument with metadata around the command
    // When the hover payload is built
    // Then the command is not repeated inside a JSON envelope
    expect(bashExtraFieldsOf(JSON.stringify({ command: 'pnpm test', timeout: 30 }))).toBe(
      '{\n  "timeout": 30\n}',
    );
  });

  it('should return undefined when the command is the only field', () => {
    expect(bashExtraFieldsOf(JSON.stringify({ command: 'pnpm test' }))).toBeUndefined();
  });
});
