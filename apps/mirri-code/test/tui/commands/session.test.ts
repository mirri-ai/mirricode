import { describe, expect, it } from 'vitest';

import { forkErrorMessage } from '#/tui/commands/index';

describe('forkErrorMessage', () => {
  it('given the engine rejects a compressed fork source, when mapped, then the restore guidance surfaces', () => {
    const error = new Error('session ses_x: compressed session must be restored before fork');

    const message = forkErrorMessage(error);

    expect(message).toContain('compressed');
    expect(message).toContain('/retention');
    expect(message).not.toContain('ses_x');
  });

  it('given any other fork failure, when mapped, then the error surfaces verbatim', () => {
    const error = new Error('fork target not found');

    expect(forkErrorMessage(error)).toBe('fork target not found');
  });
});
