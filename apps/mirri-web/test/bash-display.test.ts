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

  it('should treat a newline after the cd path as a command separator', () => {
    // Given a multi-line command where cd is separated from the next command by a bare newline
    // When the row summary is built
    // Then the cd folds into a badge and the following command leads the row
    const d = summarizeBashCommand(`cd ${WS}\ngit diff --stat`, WS);
    expect(d.prefix).toEqual({ kind: 'workspace', label: 'proj', path: WS });
    expect(d.text).toBe('git diff --stat');
  });

  it('should treat a CRLF after the cd path as a command separator', () => {
    // Given a Windows-style CRLF line ending after the cd path
    // When the row summary is built
    // Then it folds the same way as a bare LF
    const d = summarizeBashCommand(`cd ${WS}\r\ngit status`, WS);
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('git status');
  });

  it('should fold a cd to another directory when separated by a newline', () => {
    const d = summarizeBashCommand('cd /tmp/build-output\nmake all', WS);
    expect(d.prefix).toEqual({ kind: 'other', label: 'build-output', path: '/tmp/build-output' });
    expect(d.text).toBe('make all');
  });

  it('should drop a trailing comment from the collapsed command', () => {
    // Given a command carrying a model-authored trailing comment
    // When the collapsed row summary is built
    // Then only the executable part leads the row
    const d = summarizeBashCommand('git diff src/useWorkspaceState.ts # 看 git diff', WS);
    expect(d.text).toBe('git diff src/useWorkspaceState.ts');
  });

  it('should drop a trailing comment after a folded cd prefix', () => {
    const d = summarizeBashCommand(`cd ${WS} && git status # 看状态`, WS);
    expect(d.prefix?.kind).toBe('workspace');
    expect(d.text).toBe('git status');
  });

  it('should keep a # that sits inside quotes', () => {
    const d = summarizeBashCommand('echo "issue #42"', WS);
    expect(d.text).toBe('echo "issue #42"');
  });

  it('should keep a # attached to a word because it is not a comment', () => {
    const d = summarizeBashCommand('grep foo#bar src', WS);
    expect(d.text).toBe('grep foo#bar src');
  });

  it('should not lose commands that follow a commented line', () => {
    // Given a multi-line command whose first line ends in a comment
    // When the summary is built
    // Then nothing after the comment line is stripped away
    const d = summarizeBashCommand('git diff # 看\nls -la', WS);
    expect(d.text.endsWith('ls -la')).toBe(true);
  });

  it('should keep a command that is nothing but a comment', () => {
    const d = summarizeBashCommand('# 只是备注', WS);
    expect(d.text).toBe('# 只是备注');
  });

  it('should keep the trailing comment verbatim in full mode', () => {
    const cmd = 'git status # 看';
    expect(summarizeBashCommand(cmd, WS, { full: true }).text).toBe(cmd);
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
