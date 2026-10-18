// apps/mirri-web/src/lib/swarmCommand.ts
// Pure TS — no Vue, no side effects. /swarm 变体解析。

export type SwarmCommand =
  | { kind: 'toggle' }
  | { kind: 'on' }
  | { kind: 'off' }
  | { kind: 'task'; task: string };

/** Parse the argument after `/swarm` (already stripped of the command word):
 *  '' → toggle, 'on'/'off' → explicit set, anything else → a swarm task. */
export function parseSwarmCommand(arg: string): SwarmCommand {
  const trimmed = arg.trim();
  if (trimmed === '') return { kind: 'toggle' };
  if (trimmed === 'on') return { kind: 'on' };
  if (trimmed === 'off') return { kind: 'off' };
  return { kind: 'task', task: trimmed };
}
