/**
 * `sessionAgentProfileCatalog` domain — `ISessionAgentProfileCatalog`
 * implementation.
 *
 * Projects the App-scope `IAgentProfileRegistry` into this session's merged
 * profile view. The relevant entries are the global ones (builtin) plus the
 * ones tagged with the seeded workspace key (user / plugin / extra /
 * workspace / explicit); they are re-merged on every registry change (the
 * projection is a cheap full recompute — merge, never incremental patching).
 * Merge rules, applied per profile name: candidates are collected from every
 * relevant entry (deduped within an entry, highest priority first); the first
 * candidate wins — a same-name file overrides a builtin (the essential
 * default included) by virtue of existing, no declaration required
 * (adjudication D14 / ADR-006). A displaced builtin definition is recorded
 * as a suppressed candidate on the winner's row. `ready` resolves
 * immediately: the registry is already populated when this service is
 * constructed, and every later change arrives through `onDidChange`. Bound at
 * Session scope.
 */

import { Disposable } from '#/_base/di/lifecycle';
import { Emitter, type Event } from '#/_base/event';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import type { AgentProfile } from '#/app/agentProfileCatalog/agentProfileCatalog';
import { DEFAULT_AGENT_PROFILE_NAME } from '#/app/agentProfileCatalog/agentProfileCatalog';
import {
  IAgentProfileRegistry,
  type AgentProfileRegistration,
} from '#/app/agentProfileCatalog/agentProfileRegistry';
import { BUILTIN_AGENT_PROFILE_SOURCE_ID } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';

import { ISessionAgentProfileCatalogSeed } from './agentProfileCatalogSeed';
import {
  ISessionAgentProfileCatalog,
  type AgentProfileInspection,
  type AgentProfileSuppressedCandidate,
} from './sessionAgentProfileCatalog';

interface ProfileCandidate {
  readonly profile: AgentProfile;
  readonly sourceId: string;
  readonly priority: number;
}

export class SessionAgentProfileCatalogService
  extends Disposable
  implements ISessionAgentProfileCatalog
{
  declare readonly _serviceBrand: undefined;

  private merged = new Map<string, AgentProfile>();
  private all = new Map<string, AgentProfile>();
  private inspections = new Map<string, AgentProfileInspection>();
  private readonly onDidChangeEmitter = this._register(new Emitter<string>());
  readonly onDidChange: Event<string> = this.onDidChangeEmitter.event;

  constructor(
    @IAgentProfileRegistry private readonly registry: IAgentProfileRegistry,
    @ISessionAgentProfileCatalogSeed private readonly seed: ISessionAgentProfileCatalogSeed,
    @ILogService private readonly log: ILogService,
  ) {
    super();
    this.reproject();
    this._register(
      this.registry.onDidChange((change) => {
        if (change.workspaceKey !== undefined && change.workspaceKey !== this.seed.workspaceKey) {
          return;
        }
        this.reproject();
        this.onDidChangeEmitter.fire(change.sourceId);
      }),
    );
  }

  get ready(): Promise<void> {
    return Promise.resolve();
  }

  get(name: string): AgentProfile | undefined {
    return this.merged.get(name);
  }

  getDefault(): AgentProfile {
    const profile = this.get(DEFAULT_AGENT_PROFILE_NAME);
    if (profile === undefined) {
      throw new Error(
        `Default agent profile "${DEFAULT_AGENT_PROFILE_NAME}" is not registered`,
      );
    }
    return profile;
  }

  list(): readonly AgentProfile[] {
    return [...this.merged.values()];
  }

  listAll(): readonly AgentProfile[] {
    return [...this.all.values()];
  }

  inspect(name: string): AgentProfileInspection | undefined {
    return this.inspections.get(name);
  }

  async load(): Promise<void> {
    await this.ready;
  }

  async reload(): Promise<void> {
    await this.ready;
    this.reproject();
    this.onDidChangeEmitter.fire('catalog');
  }

  private relevantEntries(): AgentProfileRegistration[] {
    const key = this.seed.workspaceKey;
    return this.registry
      .entries()
      .filter((e) => e.workspaceKey === undefined || e.workspaceKey === key);
  }

  private reproject(): void {
    const merged = new Map<string, AgentProfile>();
    const disabledWinners = new Map<string, AgentProfile>();
    const inspections = new Map<string, AgentProfileInspection>();
    const entries = this.relevantEntries();

    const builtinEntry = entries.find((e) => e.sourceId === BUILTIN_AGENT_PROFILE_SOURCE_ID);
    if (builtinEntry !== undefined) {
      for (const profile of builtinEntry.contribution.profiles) {
        merged.set(profile.name, profile);
        inspections.set(profile.name, {
          name: profile.name,
          profile,
          sourceId: builtinEntry.sourceId,
          priority: builtinEntry.priority,
          suppressed: [],
        });
      }
    }

    const fileCandidates = new Map<string, ProfileCandidate[]>();
    const ordered = entries
      .filter((e) => e.sourceId !== BUILTIN_AGENT_PROFILE_SOURCE_ID)
      .toSorted((a, b) => b.priority - a.priority);
    for (const entry of ordered) {
      const entryProfiles = new Map<string, AgentProfile>();
      for (const profile of entry.contribution.profiles) {
        entryProfiles.set(profile.name, profile);
      }
      for (const profile of entryProfiles.values()) {
        const candidates = fileCandidates.get(profile.name) ?? [];
        candidates.push({
          profile,
          sourceId: entry.sourceId,
          priority: entry.priority,
        });
        fileCandidates.set(profile.name, candidates);
      }
    }

    for (const candidates of fileCandidates.values()) {
      const suppressed: AgentProfileSuppressedCandidate[] = [];
      let winner = false;
      for (const candidate of candidates) {
        if (candidate.profile.enabled === false) {
          const isEssential = candidate.profile.name === DEFAULT_AGENT_PROFILE_NAME;
          if (isEssential) {
            this.log.warn(
              `agent file profile "${candidate.profile.name}" sets enabled: false but the default agent profile is essential and cannot be disabled; treating it as enabled`,
            );
          } else {
            suppressed.push({
              sourceId: candidate.sourceId,
              priority: candidate.priority,
              reason: 'disabled',
            });
            continue;
          }
        }
        // D14 / ADR-006: a same-name file overrides by virtue of existing —
        // no `override: true` declaration is required (or read) anymore.
        // D16: when the winner displaces a seeded builtin (the essential
        // included), the builtin definition is recorded as a suppressed
        // candidate on the winner's row — `{source: builtin, priority: 0,
        // reason: priority}` — appended after the file-candidate losers so
        // the order matches the rust discovery pass. (`merged` still holds
        // the builtin seeding for this name at this point: file winners
        // set it exactly once, here.)
        const displacedBuiltin =
          builtinEntry !== undefined && merged.has(candidate.profile.name)
            ? [{
                sourceId: BUILTIN_AGENT_PROFILE_SOURCE_ID,
                priority: builtinEntry.priority,
                reason: 'priority' as const,
              }]
            : [];
        merged.set(candidate.profile.name, candidate.profile);
        inspections.set(candidate.profile.name, {
          name: candidate.profile.name,
          profile: candidate.profile,
          sourceId: candidate.sourceId,
          priority: candidate.priority,
          suppressed: [
            ...suppressed,
            ...candidates.slice(candidates.indexOf(candidate) + 1).map((rest) => ({
              sourceId: rest.sourceId,
              priority: rest.priority,
              reason: 'priority' as const,
            })),
            ...displacedBuiltin,
          ],
        });
        winner = true;
        break;
      }
      if (!winner) {
        const name = candidates[0]?.profile.name;
        if (name !== undefined && inspections.has(name)) {
          // Case a: existing winner (e.g. builtin) — just merge suppressed
          // into its inspection; do NOT record a disabled winner.
          const existing = inspections.get(name)!;
          inspections.set(name, { ...existing, suppressed });
        } else if (
          candidates[0] !== undefined &&
          candidates[0].profile.enabled === false
        ) {
          // Case b: no existing winner and the first candidate is
          // disabled — record it in disabledWinners so listAll() exposes
          // it. (An essential-named disabled candidate never reaches here:
          // the exemption above lets it win.)
          disabledWinners.set(name!, candidates[0].profile);
          inspections.set(name!, {
            name: name!,
            profile: candidates[0].profile,
            sourceId: candidates[0].sourceId,
            priority: candidates[0].priority,
            disabled: true,
            suppressed,
          });
        }
        // Case c: no-op — no existing winner and the first candidate is not
        // disabled
      }
    }

    this.merged = merged;
    this.all = new Map([...merged, ...disabledWinners]);
    this.inspections = inspections;
  }
}

registerScopedService(
  LifecycleScope.Session,
  ISessionAgentProfileCatalog,
  SessionAgentProfileCatalogService,
  ScopeActivation.OnScopeCreated,
  'sessionAgentProfileCatalog',
);
