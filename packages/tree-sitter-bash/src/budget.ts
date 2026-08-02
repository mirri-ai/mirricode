// src/budget.ts
//
// Parse budget: a hard cap on wall-clock time and on the number of syntax
// nodes a single parse may create. The parser calls `budget.tick()` every
// time it creates a node; long scan loops (word runs, strings, heredoc
// bodies) call `budget.progress()` periodically so a pathological single
// token still hits the deadline without inflating the node count. When
// either limit is exceeded the methods throw `Aborted`, which the `parse`
// entry point catches and reports as `{ ok: false, reason: 'aborted' }`.
//
// Uses `performance.now()` (monotonic, sub-millisecond precision) instead of
// `Date.now()` (wall-clock, ~1ms precision, subject to NTP skew) so the
// deadline check is accurate enough for a 50 ms budget on a loaded CI
// runner — `Date.now()` jitter caused the abort-path test to flake at
// ~125 ms when the budget was 50 ms.

// Default wall-clock budget for a single parse. 200 ms is generous enough to
// survive parallel CI runner contention (V8 JIT, GC pauses, OS scheduling
// pressure when dozens of test files run concurrently) while still catching
// accidental quadratic complexity — a pathological regression takes seconds,
// not tens of milliseconds. The abort-path performance test asserts prompt
// termination relative to this budget.
export const DEFAULT_TIMEOUT_MS = 200;
export const DEFAULT_MAX_NODES = 50_000;

/** Internal control-flow error. Never escapes the `parse` entry point. */
export class Aborted extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'Aborted';
  }
}

export interface BudgetOptions {
  /** Wall-clock limit in milliseconds. `Infinity` disables the time check. */
  timeoutMs?: number;
  /** Maximum number of nodes the parse may create. */
  maxNodes?: number;
}

export class ParseBudget {
  private readonly deadline: number;
  private readonly maxNodes: number;
  private nodeCount = 0;

  constructor(options: BudgetOptions = {}) {
    const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.deadline = performance.now() + timeoutMs;
    this.maxNodes = options.maxNodes ?? DEFAULT_MAX_NODES;
  }

  /** Number of nodes created so far. */
  get nodesUsed(): number {
    return this.nodeCount;
  }

  /**
   * Account for one created node and re-check the deadline. Throws `Aborted`
   * when the node cap is exceeded or the deadline has been reached.
   */
  tick(): void {
    this.nodeCount++;
    if (this.nodeCount > this.maxNodes) {
      throw new Aborted(`parse aborted: node budget exceeded (${this.nodeCount} > ${this.maxNodes})`);
    }
    if (performance.now() >= this.deadline) {
      throw new Aborted(`parse aborted: timeout`);
    }
  }

  /**
   * Re-check the deadline WITHOUT counting a node. For long scan loops that
   * run many iterations per produced node (character-level scanning), called
   * at intervals so the deadline is still enforced promptly.
   */
  progress(): void {
    if (performance.now() >= this.deadline) {
      throw new Aborted(`parse aborted: timeout`);
    }
  }
}
