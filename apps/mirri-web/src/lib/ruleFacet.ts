// apps/mirri-web/src/lib/ruleFacet.ts
// Pure TS — no Vue. A PREVIEW mirror of the engine's rule pattern DSL
// (rust `permission/rule_match.rs::parse_pattern`): decompose a rule
// pattern into the pieces the Settings rules panel shows. The engine stays
// the authoritative validator — this mirror exists so the panel can show
// what a rule matches and pre-check a draft before submitting it.

export interface RuleFacetView {
  /** Tool name or tool glob (e.g. "Bash" | "mcp__github__*"). */
  tool: string;
  /** The arg pattern inside the parens, when the rule narrows to args
   *  (e.g. "/foo/**"); empty parens mean none. */
  argPattern?: string;
  /** Whether the pattern is negated ("!" prefix). */
  negated: boolean;
}

/** Decompose one rule pattern; `null` when the pattern cannot parse (the
 *  engine would reject it — an unclosed paren, or no tool name left). */
export function parseRuleFacet(pattern: string): RuleFacetView | null {
  let rest = pattern.trim();
  if (rest === '') return null;
  const negated = rest.startsWith('!');
  if (negated) rest = rest.slice(1);

  const open = rest.indexOf('(');
  // No parens: the whole string is the tool name (a glob is fine).
  const tool = open === -1 ? rest : rest.slice(0, open).trim();
  // The engine's addRule rejects a pattern that names no tool (an empty
  // tool after '!' or before '(' is invalid) — mirror that here.
  if (tool === '') return null;
  if (open === -1) return { tool, negated };
  const afterOpen = rest.slice(open + 1);
  const close = afterOpen.lastIndexOf(')');
  if (close === -1) return null;
  const argPattern = afterOpen.slice(0, close).trim();
  if (argPattern === '') return { tool, negated };
  return { tool, argPattern, negated };
}
