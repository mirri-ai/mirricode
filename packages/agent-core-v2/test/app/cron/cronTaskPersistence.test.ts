/**
 * Standalone unit tests for `CronTaskPersistenceService` and `isValidCronTask`.
 *
 * Validates the App-scope persistence layer independently of the session
 * cron engine: shape guarding, CRUD round-trips, and the session-tag
 * ownership model that allows `loadFromStore` to filter/claim tasks.
 *
 * Run: ../../node_modules/.bin/vitest run test/app/cron/cronTaskPersistence.test.ts
 */
import { describe, expect, it } from 'vitest';

import { isValidCronTask, CRON_ID_REGEX } from '#/app/cron/cronTaskPersistenceService';
import type { CronTask } from '#/app/cron/cronTask';

describe('isValidCronTask', () => {
  it('accepts a well-formed recurring task', () => {
    const task: CronTask = {
      id: 'deadbeef',
      cron: '*/5 * * * *',
      prompt: 'ping',
      createdAt: 1_700_000_000_000,
      recurring: true,
    };
    expect(isValidCronTask(task)).toBe(true);
  });

  it('accepts a well-formed one-shot task', () => {
    const task: CronTask = {
      id: 'abcd1234',
      cron: '0 12 * * *',
      prompt: 'noon',
      createdAt: 1_700_000_000_000,
      recurring: false,
    };
    expect(isValidCronTask(task)).toBe(true);
  });

  it('accepts a task with optional lastFiredAt', () => {
    const task: CronTask = {
      id: 'deadbeef',
      cron: '0 9 * * *',
      prompt: 'morning',
      createdAt: 1_700_000_000_000,
      lastFiredAt: 1_700_000_360_000,
    };
    expect(isValidCronTask(task)).toBe(true);
  });

  it('accepts a task with tags', () => {
    const task: CronTask = {
      id: 'deadbeef',
      cron: '0 9 * * *',
      prompt: 'morning',
      createdAt: 1_700_000_000_000,
      tags: { sessionId: 'sess-001', owner: 'main' },
    };
    expect(isValidCronTask(task)).toBe(true);
  });

  it('accepts a ULID-format id (26 uppercase Crockford chars)', () => {
    const task: CronTask = {
      id: '01H5A3G8P4QXJK7M0NZD2R6C9V',
      cron: '0 9 * * *',
      prompt: 'morning',
      createdAt: 1_700_000_000_000,
    };
    expect(isValidCronTask(task)).toBe(true);
  });

  it('rejects null and non-objects', () => {
    expect(isValidCronTask(null)).toBe(false);
    expect(isValidCronTask(undefined)).toBe(false);
    expect(isValidCronTask('string')).toBe(false);
    expect(isValidCronTask(42)).toBe(false);
  });

  it('rejects when id is missing or not a string', () => {
    const base = { cron: '*/5 * * * *', prompt: 'p', createdAt: 0 };
    expect(isValidCronTask({ ...base, id: 123 })).toBe(false);
    expect(isValidCronTask({ ...base })).toBe(false);
  });

  it('rejects when id does not match the expected shape', () => {
    const base = { cron: '*/5 * * * *', prompt: 'p', createdAt: 0 };
    expect(isValidCronTask({ ...base, id: 'short' })).toBe(false);
    expect(isValidCronTask({ ...base, id: 'GGGGGGGG' })).toBe(false);
    expect(isValidCronTask({ ...base, id: '' })).toBe(false);
  });

  it('rejects when cron or prompt are not strings', () => {
    const base = { id: 'deadbeef', createdAt: 0 };
    expect(isValidCronTask({ ...base, cron: 5, prompt: 'p' })).toBe(false);
    expect(isValidCronTask({ ...base, cron: '*/5 * * * *', prompt: null })).toBe(false);
  });

  it('rejects when createdAt is not a number', () => {
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 'not-a-number',
      }),
    ).toBe(false);
  });

  it('rejects when recurring is not a boolean', () => {
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 0,
        recurring: 'yes',
      }),
    ).toBe(false);
  });

  it('rejects when lastFiredAt is non-finite', () => {
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 0,
        lastFiredAt: NaN,
      }),
    ).toBe(false);
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 0,
        lastFiredAt: Infinity,
      }),
    ).toBe(false);
  });

  it('rejects when tags is not a string-valued record', () => {
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 0,
        tags: { key: 123 },
      }),
    ).toBe(false);
    expect(
      isValidCronTask({
        id: 'deadbeef',
        cron: '*/5 * * * *',
        prompt: 'p',
        createdAt: 0,
        tags: null,
      }),
    ).toBe(false);
  });
});

describe('CRON_ID_REGEX', () => {
  it('matches 8-hex ids', () => {
    expect(CRON_ID_REGEX.test('deadbeef')).toBe(true);
    expect(CRON_ID_REGEX.test('abcd1234')).toBe(true);
    expect(CRON_ID_REGEX.test('00000000')).toBe(true);
  });

  it('matches 26-char ULID ids', () => {
    expect(CRON_ID_REGEX.test('01H5A3G8P4QXJK7M0NZD2R6C9V')).toBe(true);
  });

  it('rejects invalid ids', () => {
    expect(CRON_ID_REGEX.test('short')).toBe(false);
    expect(CRON_ID_REGEX.test('GGGGGGGG')).toBe(false);
    expect(CRON_ID_REGEX.test('')).toBe(false);
    expect(CRON_ID_REGEX.test('deadbee')).toBe(false);
  });
});
