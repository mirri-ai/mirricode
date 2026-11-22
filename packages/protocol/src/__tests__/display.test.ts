import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import { ToolInputDisplaySchema } from '../display';

// Cross-language contract parity: the same fixture drives the rust
// serialization test (tool.rs) and the web mapper coverage test
// (approvalDisplay.test.ts). Any dialect drift turns this red.
const fixturePath = fileURLToPath(
  new URL('../../../../rust/crates/mirri-core/test-fixtures/tool-input-display.json', import.meta.url),
);
const samples: Record<string, unknown>[] = JSON.parse(readFileSync(fixturePath, 'utf8'));

describe('ToolInputDisplaySchema wire parity', () => {
  it('should parse every canonical wire sample when the shared fixture drives the parity check', () => {
    for (const sample of samples) {
      const parsed = ToolInputDisplaySchema.safeParse(sample);
      expect(parsed.success, `sample ${JSON.stringify(sample)}`).toBe(true);
    }
  });

  it('should expose exactly the fixture kind vocabulary when schema arms are compared to the fixture', () => {
    const schemaKinds = ToolInputDisplaySchema.options.map(
      (option) => (option as unknown as { shape: { kind: { value: string } } }).shape.kind.value,
    );
    const fixtureKinds = [...new Set(samples.map((sample) => sample['kind']))];
    expect([...schemaKinds].sort()).toEqual([...fixtureKinds].sort());
  });
});
