// apps/mirri-web/test/ws-rpc-api-cron.test.ts
// Given the rust engine's cron management faces (B6: `cron.list`,
// `cron.delete`, `cron.trigger`), the WsRpcApi adapters must call exactly the
// authoritative wire methods with camelCase ids and translate the wire shapes
// back to the AppCronTask rows the Settings cron panel consumes. `cron.list`
// answers the `{tasks: [...]}` envelope (CronTasksResult), and timestamps
// stay epoch-ms end to end (no unit conversion — the panel formats them).

import { afterEach, beforeEach, beforeEach as beforeEachHook, describe, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createSSRApp, h, isReactive, reactive, ref, toRaw, type DefineComponent } from 'vue';
import { renderToString } from 'vue/server-renderer';
import { createI18n } from 'vue-i18n';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import { messages } from '../src/i18n/locales';
import { newCronDraft, scheduledTaskStatus } from '../src/lib/cronManagement';
import type { CronSchedule } from '../src/lib/cronSchedule';
import type { AppCronTask } from '../src/api/types';

// The SchedulePicker reads the API client at setup; the cases below only
// render it (no live engine), so the singleton factory answers a stub whose
// preview call never resolves against a server.
const apiMock = vi.hoisted(() => ({
  previewCronSchedule: vi.fn(() => new Promise<never>(() => {})),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));


/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

/** Build a WsRpcApi whose transport is a mocked `client.call`. */
function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>();
  (api as unknown as { client: { call: RpcCall } }).client = { call };
  return { api, call };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi cron management', () => {
  it('Given the engine answers cron.list with the {tasks: [...]} envelope, When the list is fetched, Then the rows map onto AppCronTask', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        {
          id: 'cron-1', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
          timezone: 'Asia/Shanghai', stale: false,
          createdAt: 1_700_000_000_000, lastFiredAt: null, nextFireAt: 1_700_000_300_000,
        },
      ],
    });

    const tasks = await api.listCronTasks('s-1');

    expect(call).toHaveBeenCalledWith('cron.list', { sessionId: 's-1' });
    expect(tasks).toEqual([
      {
        id: 'cron-1', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
        timezone: 'Asia/Shanghai', stale: false,
        createdAt: 1_700_000_000_000, lastFiredAt: null, nextFireAt: 1_700_000_300_000,
      },
    ]);
  });

  it('Given the engine answers cron.list with an empty {tasks: []} envelope, When the list is fetched, Then the panel gets an empty list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ tasks: [] });

    const tasks = await api.listCronTasks('s-1');

    expect(call).toHaveBeenCalledWith('cron.list', { sessionId: 's-1' });
    expect(tasks).toEqual([]);
  });

  it('Given cron.list answers a null result, When the list is fetched, Then the panel gets an empty list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const tasks = await api.listCronTasks('s-1');

    expect(tasks).toEqual([]);
  });

  it('should tolerate absent optional keys when the row is a fired one-shot', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        { id: 'cron-2', cron: '0 9 1 1 *', prompt: 'once', recurring: false,
          stale: false, createdAt: 1_700_000_000_000 },
      ],
    });

    const tasks = await api.listCronTasks('s-1');

    expect(tasks[0]).toMatchObject({ id: 'cron-2', recurring: false });
    expect(tasks[0].lastFiredAt).toBeUndefined();
    expect(tasks[0].nextFireAt).toBeUndefined();
  });

  it('should call cron.delete with the camelCase ids and map the wire ack', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true, task_id: 'cron-1' });

    const ack = await api.deleteCronTask('s-1', 'cron-1');

    expect(call).toHaveBeenCalledWith('cron.delete', { sessionId: 's-1', taskId: 'cron-1' });
    expect(ack).toEqual({ deleted: true, taskId: 'cron-1' });
  });

  it('should call cron.trigger with the camelCase ids and map the wire ack', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ triggered: true, task_id: 'cron-1' });

    const ack = await api.triggerCronTask('s-1', 'cron-1');

    expect(call).toHaveBeenCalledWith('cron.trigger', { sessionId: 's-1', taskId: 'cron-1' });
    expect(ack).toEqual({ triggered: true, taskId: 'cron-1' });
  });

  it('should fall back to the requested id when the ack omits task_id', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true });

    const ack = await api.deleteCronTask('s-1', 'cron-9');

    expect(ack).toEqual({ deleted: true, taskId: 'cron-9' });
  });

  it('should propagate a rejected cron.list call to the panel', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('session not found'));

    await expect(api.listCronTasks('s-1')).rejects.toThrow('session not found');
  });

  it('should reject when the delete ack does not confirm the removal', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: false, task_id: 'cron-1' });

    await expect(api.deleteCronTask('s-1', 'cron-1')).rejects.toThrow('did not confirm');
  });

  it('should reject when the trigger ack does not confirm the run', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ triggered: false, task_id: 'cron-1' });

    await expect(api.triggerCronTask('s-1', 'cron-1')).rejects.toThrow('did not confirm');
  });
});

describe('WsRpcApi cross-session aggregation (cron.listAll, E.5 / V8a)', () => {
  it('Given tasks spread over a live and a closed session, When the aggregated view is fetched, Then every group maps onto AppCronSessionGroup with honest open flags', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      sessions: [
        {
          sessionId: 's-live', title: 'live session', open: true,
          tasks: [{ id: 'cron-a', cron: '0 9 * * *', prompt: 'a', recurring: true,
            stale: false, createdAt: 1_700_000_000_000, origin: 'user' }],
        },
        {
          sessionId: 's-closed', title: 'closed session', open: false,
          tasks: [{ id: 'cron-b', cron: '*/10 * * * *', prompt: 'b', recurring: true,
            stale: false, createdAt: 1_700_000_000_000, origin: 'agent', maxFires: 5, fireCount: 2 }],
        },
      ],
      disabled: false,
    });

    const view = await api.listAllCronTasks();

    expect(call).toHaveBeenCalledWith('cron.listAll', {});
    expect(view.disabled).toBe(false);
    expect(view.sessions).toHaveLength(2);
    expect(view.sessions[0]).toMatchObject({ sessionId: 's-live', open: true });
    expect(view.sessions[0].tasks[0]).toMatchObject({ id: 'cron-a', origin: 'user' });
    expect(view.sessions[1]).toMatchObject({ sessionId: 's-closed', open: false });
    expect(view.sessions[1].tasks[0]).toMatchObject({ id: 'cron-b', origin: 'agent', maxFires: 5, fireCount: 2 });
  });

  it('Given a null result, When the aggregated view is fetched, Then the view degrades to empty and enabled', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const view = await api.listAllCronTasks();

    expect(view).toEqual({ sessions: [], workspaces: [], disabled: false });
  });

  it('Given scheduling is disabled, When the aggregated view is fetched, Then the disabled flag rides along honestly', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ sessions: [], disabled: true });

    const view = await api.listAllCronTasks();

    expect(view.disabled).toBe(true);
  });
});

// --- the scheduled-task page's structural contract (§5 / A1 / A6) --------
// Rendered assertions. The commands below mount the real SFCs through the
// server renderer, so every commitment is read off what the page actually
// produces (tags, classes, attributes, text) rather than off its source.
// Two files stay source-read for what a rendered tree cannot speak for:
// the scoped stylesheet (grid columns) and the absence of a form field
// whose absence is a template fact, not a runtime one — both are noted
// at the assertion that needs them.

const panelDir = join(import.meta.dirname, '../src/components/settings/scheduledTasks');
const readSfc = (name: string): string => readFileSync(join(panelDir, name), 'utf8');

/** Mount one SFC and answer its rendered HTML. */
async function renderComponent(component: unknown, props: Record<string, unknown> = {}): Promise<string> {
  const app = createSSRApp({ render: () => h(component as DefineComponent, props) });
  app.use(createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages }));
  return renderToString(app);
}

/** The rendered tree, reduced to what these cases read. */
interface RenderedNode {
  tag: string;
  classes: string[];
  attrs: Record<string, string>;
  text: string;
  children: RenderedNode[];
}

function parseRendered(html: string): RenderedNode {
  const root: RenderedNode = { tag: '#root', classes: [], attrs: {}, text: '', children: [] };
  const stack: RenderedNode[] = [root];
  const tagPattern = /<(\/?)([a-zA-Z][\w-]*)((?:\s+[^<>]*?)?)(\/?)>|<!--[\s\S]*?-->|([^<]+)/g;
  for (const match of html.matchAll(tagPattern)) {
    const [, closing, tag, rawAttrs, selfClosing, text] = match;
    if (text !== undefined) {
      (stack.at(-1) as RenderedNode).text += text;
      continue;
    }
    if (tag === undefined) continue;
    if (closing === '/') {
      if (stack.length > 1) stack.pop();
      continue;
    }
    const attrs: Record<string, string> = {};
    for (const attr of (rawAttrs ?? '').matchAll(/([\w:-]+)(?:="([^"]*)")?/g)) {
      const [, name, value] = attr;
      if (name !== undefined) attrs[name] = value ?? '';
    }
    const node: RenderedNode = {
      tag,
      classes: (attrs['class'] ?? '').split(/\s+/).filter(Boolean),
      attrs,
      text: '',
      children: [],
    };
    (stack.at(-1) as RenderedNode).children.push(node);
    if (selfClosing !== '/' && !['br', 'hr', 'img', 'input'].includes(tag)) stack.push(node);
  }
  return root;
}

function findAll(node: RenderedNode, predicate: (n: RenderedNode) => boolean): RenderedNode[] {
  const found: RenderedNode[] = [];
  const walk = (current: RenderedNode): void => {
    if (predicate(current)) found.push(current);
    for (const child of current.children) walk(child);
  };
  walk(node);
  return found;
}

/** The node's whole subtree as text (what a reader sees). */
function textOf(node: RenderedNode): string {
  return (node.text + node.children.map(textOf).join(' ')).replaceAll(/\s+/g, ' ').trim();
}

/** A task row shaped like the panel's list rows: a REACTIVE object, the way
 *  the live list stores what the panel hands to the detail view. */
function taskRow(overrides: Record<string, unknown> = {}): AppCronTask {
  return reactive({
    id: 'cron-1',
    cron: '0 9 * * *',
    prompt: '每天汇总构建结果',
    recurring: true,
    stale: false,
    createdAt: 1_700_000_000_000,
    nextFireAt: 1_700_000_300_000,
    permissionMode: 'manual',
    paused: false,
    origin: 'user',
    ...overrides,
  }) as AppCronTask;
}

describe('ScheduledTaskPanel structure (§5 / A1 / A6 / A12)', () => {
  it('Given a task, When its card renders, Then the whole card is ONE button and carries no action or badge (§5)', async () => {
    const { default: ScheduledTaskCard } = await import('../src/components/settings/scheduledTasks/ScheduledTaskCard.vue');
    const tree = parseRendered(
      await renderComponent(ScheduledTaskCard, { task: taskRow(), now: 1_700_000_100_000 }),
    );
    const cards = findAll(tree, (n) => n.classes.includes('task-card'));
    expect(cards).toHaveLength(1);
    const card = cards[0] as RenderedNode;
    // The card itself is the button (one click into the detail view).
    expect(card.tag).toBe('button');
    expect(findAll(card, (n) => n.tag === 'button' && n !== card)).toEqual([]);
    // No status badge and none of the per-card actions §5 bans — edit /
    // pause / history live in the detail view, status in the filter tabs.
    expect(findAll(card, (n) => n.classes.some((c) => /badge/i.test(c)))).toEqual([]);
    expect(findAll(card, (n) => n.attrs['role'] === 'button' && n !== card)).toEqual([]);
    expect(JSON.stringify(card)).not.toMatch(/scheduledTask(Pause|Resume|Delete|RunNow)/);
    // The page's source carries no other <button in the card template.
    expect(readSfc('ScheduledTaskCard.vue').match(/<button/g)?.length ?? 0).toBe(1);
  });

  it('Given the form, When rendered, Then the four secondary pickers share ONE unlabelled toolbar (§5)', async () => {
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const tree = parseRendered(
      await renderComponent(ScheduledTaskForm, {
        initialDraft: { ...newCronDraft('ws-1'), schedule: { kind: 'daily', time: '09:00' } },
        mode: 'create',
        workspaces: [{ id: 'ws-1', name: '工作区' }],
        models: [],
        submitting: false,
      }),
    );
    const toolbars = findAll(tree, (n) => n.classes.includes('stf-config-row'));
    expect(toolbars).toHaveLength(1);
    const toolbar = toolbars[0] as RenderedNode;
    // One select per picker — workspace / permission mode / model / effort —
    // each naming itself through aria-label, with no visible label line.
    expect(findAll(toolbar, (n) => n.tag === 'select')).toHaveLength(4);
    expect(findAll(toolbar, (n) => n.classes.includes('stf-label'))).toEqual([]);
    const labels = findAll(toolbar, (n) => n.attrs['aria-label'] !== undefined);
    expect(labels).toHaveLength(4);
    // A3: no working-directory input anywhere in the form (a template fact —
    // the rendered tree cannot show a field that was never authored).
    expect(readSfc('ScheduledTaskForm.vue')).not.toMatch(/\bworkdir\b/);
  });

  it('Given a reactive draft, When the form mounts, Then it renders instead of crashing on the snapshot copy', async () => {
    // The parent passes the draft through a Vue `ref` (deep reactive Proxy).
    // `structuredClone` cannot clone a Proxy, so the setup function used to
    // throw DataCloneError and the whole form rendered blank.
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const html = await renderComponent(ScheduledTaskForm, {
      initialDraft: reactive(newCronDraft('ws-1')),
      mode: 'create',
      workspaces: [{ id: 'ws-1', name: '工作区' }],
      models: [],
      submitting: false,
    });
    const tree = parseRendered(html);
    // The mounted form carries its fields — title, schedule, prediction hint,
    // and instruction — instead of a crashed empty shell.
    expect(findAll(tree, (n) => n.classes.includes('stf-field'))).toHaveLength(4);
    expect(html).toContain('任务标题');
    expect(html).toContain('指示');
  });

  it('Given an edit task stored reactively, When the form mounts, Then the snapshot copy succeeds through the draft\'s reactive layers', async () => {
    // The edit path copies a `draftFromTask` draft; deriving it from a
    // REACTIVE task (the live list's row) is exactly what the detail view
    // does, and the reactive schedule the copy walks is the same proxy
    // shape the create path hands over.
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const { draftFromTask } = await import('../src/lib/cronManagement');
    const task = taskRow({ cron: '*/5 * * * *', timezone: 'Asia/Tokyo', title: '巡检' });
    const initialDraft = reactive(draftFromTask(task, 'UTC'));
    expect(isReactive(initialDraft.schedule)).toBe(true);

    const html = await renderComponent(ScheduledTaskForm, {
      initialDraft,
      mode: 'edit',
      workspaces: [{ id: 'ws-1', name: '工作区' }],
      models: [],
      submitting: false,
    });
    const tree = parseRendered(html);
    expect(findAll(tree, (n) => n.classes.includes('stf-field'))).toHaveLength(4);
    // The edit mode prefills from the task: the title, the zone, and the
    // read-only workspace chip instead of the create-mode select.
    expect(html).toContain('巡检');
    expect(html).toContain('Asia/Tokyo');
    expect(findAll(tree, (n) => n.attrs['data-testid'] === 'workspace-locked')).toHaveLength(1);
  });

  it('Given a reactive custom rule, When the picker copies it to reopen the custom editor, Then the copy succeeds through the proxy', async () => {
    // `openCustomDialog` runs on a click (SSR cannot drive it), so the
    // custom rule is handed over the same way the model does it — as a
    // reactive value — and the shipped source is pinned to unwrap it.
    expect(readSfc('SchedulePicker.vue')).toMatch(/structuredClone\(toRaw\(current\)\)/);
    const schedule = reactive<CronSchedule>({
      kind: 'custom',
      mode: 'raw',
      raw: { expr: '*/5 * * * *' },
      ends: { kind: 'never' },
    });
    expect(() => structuredClone(schedule)).toThrow();
    const copy = structuredClone(toRaw(schedule));
    expect(copy).toEqual({ kind: 'custom', mode: 'raw', raw: { expr: '*/5 * * * *' }, ends: { kind: 'never' } });
    expect(isReactive(copy)).toBe(false);
  });

  it('Given the preset time selects, When the picker offers times, Then every minute of the hour is selectable in two segments (SDR-117)', async () => {
    // The preset sub-option time is an hour select + a minute select sharing
    // the full 0-59 minute range; a 15-minute grid silently forbade, say,
    // 09:07 on a 每天 schedule.
    const pickerSource = readSfc('SchedulePicker.vue');
    expect(pickerSource).toMatch(/const minuteTiles = minuteOptions\.map\(/);
    expect(pickerSource).toMatch(/const hourTiles = hourOptions\.map\(/);
    expect(pickerSource).not.toMatch(/\['00', '15', '30', '45'\]/);
  });

  it('Given the four preset time pickers, When rendered, Then each one is an hour select + a minute select instead of one 1440-row list', async () => {
    // A single <select> holding all 1440 HH:MM values is a scroll obligation.
    // The pair writes back as one `HH:MM` through the same `patchSchedule({ time })`.
    const pickerSource = readSfc('SchedulePicker.vue');
    expect(pickerSource).not.toMatch(/timeOptions/);
    expect(pickerSource).not.toMatch(/flatMap\(/);
    // daily / weekday, weekly and monthly each carry the two segments.
    expect(pickerSource.match(/:aria-label="t\('settings\.cronRuleAtHour'\)"/g)).toHaveLength(3);
    expect(pickerSource.match(/@update:model-value="patchTimeSegment\(schedule\.time, 'hour', \$event\)"/g)).toHaveLength(3);
    expect(pickerSource.match(/@update:model-value="patchTimeSegment\(schedule\.time, 'minute', \$event\)"/g)).toHaveLength(3);

    const { default: SchedulePicker } = await import('../src/components/settings/scheduledTasks/SchedulePicker.vue');
    for (const schedule of [
      { kind: 'daily' as const, time: '09:07' },
      { kind: 'weekday' as const, time: '09:07' },
      { kind: 'weekly' as const, days: [1], time: '09:07' },
      { kind: 'monthly' as const, day: 1, time: '09:07' },
    ]) {
      const tree = parseRendered(
        await renderComponent(SchedulePicker, { schedule, timezone: 'Asia/Tokyo' }),
      );
      const hourSelect = findAll(
        tree,
        (n) => n.tag === 'select' && n.attrs['aria-label'] === '小时',
      );
      expect(hourSelect).toHaveLength(1);
      // The stored value's segments are each preselected (SSR writes the
      // chosen value onto the <select>'s own `value`, and the browser picks
      // the matching option from it).
      expect((hourSelect[0] as RenderedNode).attrs['value']).toBe('09');
      // Every hour is offered once — 24 rows, not 1440.
      expect(findAll(hourSelect[0] as RenderedNode, (n) => n.tag === 'option')).toHaveLength(24);
      const minuteSelect = findAll(
        tree,
        (n) => n.tag === 'select' && n.attrs['aria-label'] === '第几分',
      );
      expect(minuteSelect).toHaveLength(1);
      expect((minuteSelect[0] as RenderedNode).attrs['value']).toBe('07');
      expect(findAll(minuteSelect[0] as RenderedNode, (n) => n.tag === 'option')).toHaveLength(60);
    }
  });

  it('Given a one-shot task that fired, When the status word renders, Then it reads 已结束·按计划结束 not 已完成', async () => {
    // The status VALUE stays 'completed' (filter plumbing untouched); the
    // DISPLAY word changes (SDR-120).
    const oneShotFired = taskRow({ recurring: false, fireCount: 1, nextFireAt: undefined });
    expect(scheduledTaskStatus(oneShotFired)).toBe('completed');
    const { default: ScheduledTaskCard } = await import('../src/components/settings/scheduledTasks/ScheduledTaskCard.vue');
    const html = await renderComponent(ScheduledTaskCard, { task: oneShotFired, now: 1_700_000_100_000 });
    expect(html).toContain('已结束 · 按计划结束');
    expect(html).not.toContain('已完成');
  });

  it('Given an ended task card with zero runs, When rendered, Then it reads 已结束·按计划结束 and 尚未运行过', async () => {
    const endedZeroRun = taskRow({
      cron: '0 9 1 1 *',
      recurring: false,
      fireCount: 0,
      nextFireAt: undefined,
      nextTwoFires: undefined,
    });
    const { default: ScheduledTaskCard } = await import('../src/components/settings/scheduledTasks/ScheduledTaskCard.vue');
    const html = await renderComponent(ScheduledTaskCard, { task: endedZeroRun, now: 1_700_000_100_000 });
    expect(html).toContain('已结束 · 按计划结束');
    expect(html).toContain('尚未运行过');
    expect(html).not.toContain('已运行 0 次');
  });

  it('Given a confirmed custom rule, When the editor reopens, Then the stored model value carries no nested proxies', async () => {
    // The write-back after confirming a custom rule must deep-copy the
    // reactive scratch draft: a shallow spread leaves the nested `ends` /
    // `raw` / `field` objects as proxies inside the model, and the NEXT
    // openCustomDialog then re-clones them — DataCloneError again.
    expect(readSfc('SchedulePicker.vue')).toMatch(/const value: CustomSchedule = structuredClone\(toRaw\(draft\)\)/);

    // Replay the real data flow with the repo's own reactivity: the scratch
    // draft is a deep reactive ref, the confirm deep-copies it into the
    // model, and the reopen clones the model value.
    const customDraft = ref<CronSchedule | null>({
      kind: 'custom',
      mode: 'raw',
      raw: { expr: '*/5 * * * *' },
      ends: { kind: 'on', date: '2026-12-31' },
    });
    const model = ref<CronSchedule | null>(null);

    // confirm: onCustomConfirm's write-back
    const draft = customDraft.value;
    if (draft !== null && draft.kind === 'custom') {
      model.value = structuredClone(toRaw(draft));
    }
    // reopen: openCustomDialog's copy
    const current = model.value;
    const reopened = current !== null ? structuredClone(toRaw(current)) : null;
    expect(reopened).toEqual({
      kind: 'custom',
      mode: 'raw',
      raw: { expr: '*/5 * * * *' },
      ends: { kind: 'on', date: '2026-12-31' },
    });
    expect(isReactive(reopened?.ends ?? null)).toBe(false);
  });

  it('Given the page, When rendered, Then the create entry has no precondition beyond the v3 engine (A1)', async () => {
    // The A1 gate is structural: the create button is rendered without any
    // disabled state. (The page's own render needs a live API — see the
    // note on this suite — so the button's shape is pinned on its source
    // contract AND the card/form above prove the rendered pieces.)
    const panelSource = readSfc('ScheduledTaskPanel.vue');
    const createAt = panelSource.indexOf('@click="openCreate"');
    const buttonStart = panelSource.lastIndexOf('<Button', createAt);
    const createButton = panelSource.slice(buttonStart, createAt);
    expect(createButton).not.toMatch(/:disabled=/);
    expect(createButton).toMatch(/variant="primary"/);
  });

  it('Given a blank create form, When it first renders, Then no validation error is visible (SDR-114)', async () => {
    // SDR-114 a: a pristine form shows zero errors — the required-prompt and
    // the limit messages appear only AFTER the field has been offended.
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const html = await renderComponent(ScheduledTaskForm, {
      initialDraft: newCronDraft('ws-1'),
      mode: 'create',
      workspaces: [{ id: 'ws-1', name: '工作区' }],
      models: [],
      submitting: false,
    });
    expect(html).not.toContain('请填写任务的指示内容');
    expect(parseRendered(html).children.flatMap((n) => findAll(n, (x) => x.classes.includes('stf-error')))).toEqual([]);
  });

  it('Given the edit-mode action row, When the stylesheet is read, Then it sticks at bottom 0 and the form gives up its bottom padding (SDR-118 / VU23)', async () => {
    // A negative `bottom` cancelled the form's bottom padding but let the
    // row's lower edge be pushed out of the scroll container before it
    // stuck — the buttons were clipped. The row now sticks at `bottom: 0`,
    // and the form's own bottom padding is dropped in edit mode so the
    // sticky line lands on the visible edge.
    const formSource = readSfc('ScheduledTaskForm.vue');
    expect(formSource).toMatch(/\.stf-actions\.is-sticky\s*\{[^}]*position: sticky;[^}]*bottom: 0;/);
    expect(formSource).not.toMatch(/bottom: calc\(-1 \* var\(--space-6\)\)/);
    expect(formSource).toMatch(/\.stf\.is-edit\s*\{\s*padding-bottom: 0;/);
    // The class rides on the root only in edit mode.
    expect(formSource).toMatch(/:class="\{ 'is-edit': mode === 'edit' \}"/);
    const tree = parseRendered(
      await renderComponent((await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue')).default, {
        initialDraft: { ...newCronDraft('ws-1'), schedule: { kind: 'daily', time: '09:00' } },
        mode: 'create',
        workspaces: [{ id: 'ws-1', name: '工作区' }],
        models: [],
        submitting: false,
      }),
    );
    const roots = findAll(tree, (n) => n.tag === 'form');
    expect(roots[0]?.classes).not.toContain('is-edit');
  });

  it('Given a blurred limit field, When the input is cleared, Then the error clears because 不填 = 不限 (SDR-114 c)', async () => {
    // The touched guard only gates WHEN the check runs; the check itself
    // answers「filled but illegal」. '' means "no limit" and is legal, so a
    // cleared field must not keep glowing red after the user has fixed it.
    const formSource = readSfc('ScheduledTaskForm.vue');
    expect(formSource).toMatch(/if \(text === '' \|\| !recurring\.value\) return false;/);
    expect(formSource).toMatch(/if \(!maxFiresTouched\.value && !maxFiresInheritedInvalid\.value\) return null;/);
    expect(formSource).toMatch(/return isMaxFiresInvalid\(draft\.value\.maxFiresText\.trim\(\)\)/);
  });

  it('Given a blank create form, When the user tries to submit, Then the empty prompt is challenged instead of being silently blocked', async () => {
    // SDR-114 b: a submit attempt IS an offence. An empty prompt must not
    // disable the submit button — it is rejected on click, with the error
    // shown in place — so the touched state and the on-submit judgement are
    // both pinned on the source.
    const formSource = readSfc('ScheduledTaskForm.vue');
    expect(formSource).toMatch(/promptTouched/);
    expect(formSource).toMatch(/maxFiresTouched/);
    // The shared pure function still demands a prompt; the form's own
    // `submittable` must not inherit that demand (the click stays reachable).
    expect(formSource).not.toMatch(/cronDraftSubmittable\(draft\.value\)/);
    expect(formSource).toMatch(/promptError\.value !== null\) return/);
  });

  it('Given an edit task whose stored limit is no longer valid, When the form mounts, Then the error and its repair hint are shown up front (SDR-114 d)', async () => {
    // SDR-114 d: an inherited offence. The saved value is invalid before the
    // user touches anything — opening the form says so, and the message
    // carries the repair guidance (which field, why, fix before saving).
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const { draftFromTask } = await import('../src/lib/cronManagement');
    const html = await renderComponent(ScheduledTaskForm, {
      initialDraft: draftFromTask(taskRow({ cron: '0 9 * * *', maxFires: 0, fireCount: 0 }), 'UTC'),
      mode: 'edit',
      workspaces: [{ id: 'ws-1', name: '工作区' }],
      models: [],
      submitting: false,
    });
    expect(html).toContain('上限必须是不小于 1 的整数');
    expect(html).toContain('该任务保存的这项配置已不合法，请修正后保存');
  });

  it('Given a task whose stored value is valid, When the edit form mounts, Then no inherited error is invented (SDR-114 d)', async () => {
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const { draftFromTask } = await import('../src/lib/cronManagement');
    const html = await renderComponent(ScheduledTaskForm, {
      initialDraft: draftFromTask(taskRow({ cron: '0 9 * * *', maxFires: 10, fireCount: 0 }), 'UTC'),
      mode: 'edit',
      workspaces: [{ id: 'ws-1', name: '工作区' }],
      models: [],
      submitting: false,
    });
    expect(html).not.toContain('上限必须是不小于 1 的整数');
    expect(html).not.toContain('该任务保存的这项配置已不合法，请修正后保存');
  });

  it('Given a blank create form, When the four chips render, Then they and the actions sit in separate visual groups (SDR-115)', async () => {
    // SDR-115: the secondary choices keep their unlabelled one-line group;
    // cancel / submit form their own row so the primary action is not wedged
    // between pickers.
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const tree = parseRendered(
      await renderComponent(ScheduledTaskForm, {
        initialDraft: newCronDraft('ws-1'),
        mode: 'create',
        workspaces: [{ id: 'ws-1', name: '工作区' }],
        models: [],
        submitting: false,
      }),
    );
    const configRows = findAll(tree, (n) => n.classes.includes('stf-config-row'));
    expect(configRows).toHaveLength(1);
    expect(findAll(configRows[0] as RenderedNode, (n) => n.tag === 'select')).toHaveLength(4);
    expect(findAll(configRows[0] as RenderedNode, (n) => n.tag === 'button')).toEqual([]);
    const actionRows = findAll(tree, (n) => n.classes.includes('stf-actions'));
    expect(actionRows).toHaveLength(1);
    expect(findAll(actionRows[0] as RenderedNode, (n) => n.tag === 'button')).toHaveLength(2);
  });

  it('Given the form in edit mode, When it renders, Then the action row is sticky so save stays reachable (SDR-118)', async () => {
    const { default: ScheduledTaskForm } = await import('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue');
    const { draftFromTask } = await import('../src/lib/cronManagement');
    const tree = parseRendered(
      await renderComponent(ScheduledTaskForm, {
        initialDraft: draftFromTask(taskRow(), 'UTC'),
        mode: 'edit',
        workspaces: [{ id: 'ws-1', name: '工作区' }],
        models: [],
        submitting: false,
      }),
    );
    const sticky = findAll(tree, (n) => n.classes.includes('stf-actions') && n.classes.includes('is-sticky'));
    expect(sticky).toHaveLength(1);
    const formSource = readSfc('ScheduledTaskForm.vue');
    expect(formSource).toMatch(/'is-sticky': mode === 'edit'/);
    expect(formSource).toMatch(/\.stf-actions\.is-sticky/);
    expect(formSource).toMatch(/position: sticky/);
  });

  it('Given the page, When rendered, Then no session-perspective control exists (A6)', async () => {
    // A6 is a removal: the four retired controls must not exist in ANY of
    // the page's components. The rendered card above shows the list face
    // carries no search box; the source sweep covers the rest.
    const pageSources = [
      'ScheduledTaskPanel.vue',
      'ScheduledTaskCard.vue',
      'ScheduledTaskForm.vue',
    ]
      .map(readSfc)
      .join('\n');
    expect(pageSources).not.toMatch(/cronViewAll|cronViewSession|cronSearch|ToolbarSearch/);
    expect(pageSources).not.toMatch(/<input[^>]*type="search"/);
  });

  it('Given the prediction block, When rendered, Then the caption appears exactly once (P2-1)', async () => {
    // The form owns the caption; the timeline it embeds must not print a
    // second title above the same points.
    expect(readSfc('ScheduleTimeline.vue')).not.toMatch(/prediction-caption/);
    expect(readSfc('ScheduledTaskForm.vue')).toMatch(/stf-prediction-caption/);
  });

  it('Given the card metadata row, When styled, Then the facts cluster from the left instead of pinning both edges (SDR-119)', async () => {
    const cardSource = readSfc('ScheduledTaskCard.vue');
    const metaRule = cardSource.slice(cardSource.indexOf('.task-card-meta {'));
    expect(metaRule.slice(0, metaRule.indexOf('}'))).not.toMatch(/space-between/);
    expect(cardSource).toMatch(/\.task-card-meta \{[^}]*gap: var\(--space-3\)/);
  });
});

// --- A14 batch B: the aggregate panel (every workspace's tasks) -------------

describe('ScheduledTaskPanel aggregate grouping (A14)', () => {
  it('Given multiple workspaces with tasks, When the panel groups them, Then the aggregate covers every workspace with the active one first', async () => {
    // The grouping/sorting logic is a pure function of (allTasks, workspaces, activeId) —
    // extract it to apps/mirri-web/src/lib/cronManagement.ts as
    // `groupTasksByWorkspace(tasks, workspaceNames, activeWorkspaceId)` and test it directly.
    const { groupTasksByWorkspace } = await import('../src/lib/cronManagement');
    const task = (id: string, ws: string) => ({
      ...taskRow(), id, scope: { kind: 'workspace' as const, workspaceId: ws },
    });
    const groups = groupTasksByWorkspace(
      [task('t1', 'ws-b'), task('t2', 'ws-a'), task('t3', 'ws-b')],
      [{ id: 'ws-a', name: 'Alpha' }, { id: 'ws-b', name: 'Beta' }],
      'ws-a',
    );
    expect(groups.map((g) => g.workspaceId)).toEqual(['ws-a', 'ws-b']);
    expect(groups[1]?.tasks.map((t) => t.id)).toEqual(['t1', 't3']);
    // empty workspace produces no group
    expect(groupTasksByWorkspace([], [{ id: 'ws-a', name: 'Alpha' }], 'ws-a')).toEqual([]);
  });

  it('Given a task without a workspace scope, When the panel groups, Then it falls back to the active workspace', async () => {
    const { groupTasksByWorkspace } = await import('../src/lib/cronManagement');
    const groups = groupTasksByWorkspace(
      [taskRow({ id: 't-no-scope' })],
      [{ id: 'ws-a', name: 'Alpha' }, { id: 'ws-b', name: 'Beta' }],
      'ws-b',
    );
    expect(groups).toHaveLength(1);
    expect(groups[0]?.workspaceId).toBe('ws-b');
    expect(groups[0]?.workspaceName).toBe('Beta');
    expect(groups[0]?.tasks.map((t) => t.id)).toEqual(['t-no-scope']);
  });

  it('Given workspaces whose names collide, When the panel groups, Then the sort stays deterministic by workspace id', async () => {
    const { groupTasksByWorkspace } = await import('../src/lib/cronManagement');
    const task = (id: string, ws: string) => ({
      ...taskRow(), id, scope: { kind: 'workspace' as const, workspaceId: ws },
    });
    const groups = groupTasksByWorkspace(
      [task('t1', 'ws-b'), task('t2', 'ws-a')],
      [{ id: 'ws-a', name: 'Same' }, { id: 'ws-b', name: 'Same' }],
      'ws-z',
    );
    expect(groups.map((g) => g.workspaceId)).toEqual(['ws-a', 'ws-b']);
  });

  it('Given the list template, When the panel renders, Then every workspace gets its own group section (A14)', () => {
    // Structural: one <section class="stp-group"> per workspace group with a
    // heading, the two-column grid inside, and a per-group filter-empty line —
    // never a single flat grid.
    const panelSource = readSfc('ScheduledTaskPanel.vue');
    expect(panelSource).toContain('v-for="group in taskGroups"');
    expect(panelSource).toMatch(/class="stp-group"/);
    expect(panelSource).toMatch(/class="stp-group-title"/);
    expect(panelSource).toContain("t('settings.scheduledTaskGroupHeading', { name: group.workspaceName })");
    expect(panelSource).toMatch(/visibleTasksOf\(group\)/);
    expect(panelSource).toMatch(/v-if="visibleTasksOf\(group\)\.length === 0"/);
    // The single-workspace pick is gone — the list reads every group.
    expect(panelSource).not.toMatch(/groups\.find/);
  });

  it('Given no tasks in any workspace, When the list renders, Then the empty state speaks for the whole aggregate (A14)', () => {
    const panelSource = readSfc('ScheduledTaskPanel.vue');
    expect(panelSource).toContain('allTasks.length === 0');
    expect(panelSource).toContain('scheduledTaskEmptyAllTitle');
    expect(panelSource).toContain('scheduledTaskEmptyAllHint');
    // The old single-workspace empty copy no longer drives the list's state.
    expect(panelSource).not.toContain("t('settings.scheduledTaskEmptyTitle')");
  });

  it('Given the grouped list, When the i18n catalog is read, Then the three new keys exist in zh and en', async () => {
    const { messages } = await import('../src/i18n/locales');
    const zh = (messages.zh as Record<string, Record<string, string>>).settings;
    const en = (messages.en as Record<string, Record<string, string>>).settings;
    for (const catalog of [zh, en]) {
      expect(typeof catalog['scheduledTaskGroupHeading']).toBe('string');
      expect(catalog['scheduledTaskGroupHeading']).toContain('{name}');
      expect(typeof catalog['scheduledTaskEmptyAllTitle']).toBe('string');
      expect(typeof catalog['scheduledTaskEmptyAllHint']).toBe('string');
    }
  });
});

// --- stage-2 batch-5: workspace faces, proposals, run records ---------------

describe('WsRpcApi workspace faces (stage-2 D.0c / E.1)', () => {
  it('Given a workspace aggregate with tasks, When listAllCronTasks is fetched, Then the workspace groups map onto AppCronWorkspaceGroup rows', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      sessions: [],
      workspaces: [
        {
          workspaceId: 'ws-1',
          tasks: [
            {
              id: 'cron-w1', cron: '0 9 * * *', prompt: 'sweep', recurring: true,
              stale: false, createdAt: 1,
              scope: { kind: 'workspace', workspaceId: 'ws-1' },
              permissionMode: 'yolo',
              concurrency: 'queue',
              paused: true,
              deliveryCondition: '服务运行即触发',
            },
          ],
        },
      ],
      disabled: false,
    });
    const view = await api.listAllCronTasks();
    expect(view.workspaces).toHaveLength(1);
    const task = view.workspaces[0]?.tasks[0];
    expect(task?.scope).toEqual({ kind: 'workspace', workspaceId: 'ws-1' });
    expect(task?.permissionMode).toBe('yolo');
    expect(task?.concurrency).toBe('queue');
    expect(task?.paused).toBe(true);
    expect(task?.deliveryCondition).toBe('服务运行即触发');
  });

  it('Given a permission mode, When createWorkspaceCronTask is called, Then cron.add carries it verbatim (A4: no posture fold)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ taskId: 'cron-new', cron: '0 9 * * *', recurring: true });
    await api.createWorkspaceCronTask('ws-1', {
      cron: '0 9 * * *', prompt: 'p', recurring: true,
      permissionMode: 'yolo', concurrency: 'queue',
    });
    expect(call).toHaveBeenCalledWith('cron.add', {
      workspaceId: 'ws-1',
      cron: '0 9 * * *', prompt: 'p', recurring: true,
      permissionMode: 'yolo', concurrency: 'queue',
    });
  });

  it('Given no mode / concurrency, When the workspace create is called, Then those keys stay absent (documented defaults)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ taskId: 'cron-new', cron: '0 9 * * *', recurring: true });
    await api.createWorkspaceCronTask('ws-1', { cron: '0 9 * * *', prompt: 'p', recurring: true });
    const params = call.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(params['permissionMode']).toBeUndefined();
    expect(params['concurrency']).toBeUndefined();
    // C2/A4: the retired input faces never ride the wire.
    expect(params).not.toHaveProperty('unattended');
    expect(params['permissionPosture']).toBeUndefined();
  });

  it('Given a workspace task delete/trigger, When called, Then the workspaceId arms route the same wire methods', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true, task_id: 'cron-w1' });
    await api.deleteWorkspaceCronTask('ws-1', 'cron-w1');
    expect(call).toHaveBeenCalledWith('cron.delete', { workspaceId: 'ws-1', taskId: 'cron-w1' });
    call.mockResolvedValue({ triggered: true, task_id: 'cron-w1' });
    await api.triggerWorkspaceCronTask('ws-1', 'cron-w1');
    expect(call).toHaveBeenCalledWith('cron.trigger', { workspaceId: 'ws-1', taskId: 'cron-w1' });
  });
});

describe('WsRpcApi run records face (stage-2 C.2 / B.2)', () => {  it('Given a task with runs, When listCronRuns is called, Then the rows map with the summary, asked ops, and the session ref', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      runs: [
        {
          runId: 'run-1', status: 'succeeded',
          startedAt: '2026-10-31T01:00:00Z', endedAt: '2026-10-31T01:02:00Z', durationMs: 120000,
          usage: { summary: '巡检完成：3 项异常' },
          sessionRef: 'sess-run-1', skipReason: null,
          askedOps: [{ tool: 'Write', decision: 'rejected', reason: 'read-only posture' }],
        },
        {
          runId: 'run-2', status: 'skipped',
          startedAt: '2026-10-31T02:00:00Z', endedAt: '2026-10-31T02:00:00Z', durationMs: 0,
          usage: null, sessionRef: null, skipReason: '上次运行中',
        },
      ],
    });
    const runs = await api.listCronRuns('ws-1', 'cron-w1');
    expect(call).toHaveBeenCalledWith('cron.runs.list', { workspaceId: 'ws-1', taskId: 'cron-w1' });
    expect(runs).toHaveLength(2);
    expect(runs[0]?.usage?.summary).toBe('巡检完成：3 项异常');
    expect(runs[0]?.askedOps?.[0]?.tool).toBe('Write');
    expect(runs[1]?.skipReason).toBe('上次运行中');
  });

  it('Given run rows carrying the launch record, When listCronRuns is called, Then trigger, schedule moment, delay and waiting_approval map through', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      runs: [
        {
          runId: 'run-3', status: 'waiting_approval',
          startedAt: '2026-11-04T01:22:57Z', endedAt: null, durationMs: null,
          usage: null, sessionRef: 'sess-run-3', skipReason: null,
          trigger: 'scheduled', scheduledAt: 1793754900000, launchDelayMs: 477000,
        },
        {
          runId: 'run-4', status: 'succeeded',
          startedAt: '2026-10-31T01:00:00Z', endedAt: '2026-10-31T01:02:00Z', durationMs: 120000,
          usage: null, sessionRef: 'sess-run-4', skipReason: null,
        },
      ],
    });
    const runs = await api.listCronRuns('ws-1', 'cron-w1');
    expect(runs[0]?.status).toBe('waiting_approval');
    expect(runs[0]?.trigger).toBe('scheduled');
    expect(runs[0]?.scheduledAt).toBe(1793754900000);
    expect(runs[0]?.launchDelayMs).toBe(477000);
    // A pre-launch-record row (no such keys on the wire) maps to nulls.
    expect(runs[1]?.trigger).toBeNull();
    expect(runs[1]?.scheduledAt).toBeNull();
    expect(runs[1]?.launchDelayMs).toBeNull();
  });

  it('Given an open request, When openCronRun is called, Then cron.run.open rides the workspace + session ids', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ sessionId: 'sess-run-1', materialized: true });
    const result = await api.openCronRun('ws-1', 'sess-run-1');
    expect(call).toHaveBeenCalledWith('cron.run.open', { workspaceId: 'ws-1', sessionId: 'sess-run-1' });
    expect(result.materialized).toBe(true);
  });
});

// --- form batch 6: cron.preview + the task-form projection -----------------

describe('WsRpcApi schedule preview face (A5 / B1 — the prediction timeline)', () => {
  it('Given a cron expression, When previewCronSchedule is called, Then cron.preview rides the expression + options and the answer maps onto occurrences + previousT0', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      occurrences: [1_700_000_300_000, 1_700_000_600_000],
      previousT0: 1_699_999_900_000,
    });

    const preview = await api.previewCronSchedule(
      { cron: '*/5 * * * *' },
      { timezone: 'Asia/Shanghai', count: 2 },
    );

    expect(call).toHaveBeenCalledWith('cron.preview', {
      cron: '*/5 * * * *',
      timezone: 'Asia/Shanghai',
      count: 2,
    });
    expect(preview).toEqual({
      occurrences: [1_700_000_300_000, 1_700_000_600_000],
      previousT0: 1_699_999_900_000,
    });
  });

  it('Given a one-off-shaped rule (no theoretical past), When previewCronSchedule answers, Then previousT0 stays null (the UI dash)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ occurrences: [1_700_000_300_000], previousT0: null });
    const preview = await api.previewCronSchedule({ cron: '0 9 2 11 *' });
    expect(preview.previousT0).toBeNull();
  });

  it('Given an interval schedule, When previewCronSchedule is called, Then the interval rides the wire (no cron key)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ occurrences: [1_700_000_300_000], previousT0: null });

    await api.previewCronSchedule({
      interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
    });

    expect(call).toHaveBeenCalledWith('cron.preview', {
      interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
    });
  });

  it('Given a null answer, When previewCronSchedule is called, Then the honest empty answer degrades (no points, no past)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);
    const preview = await api.previewCronSchedule({ cron: 'bad expression' });
    expect(preview).toEqual({ occurrences: [], previousT0: null });
  });

  it('Given an anchor option, When previewCronSchedule is called, Then the anchor rides the wire verbatim (the caller pins the walk origin)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ occurrences: [1_700_000_300_000], previousT0: null });
    await api.previewCronSchedule(
      { interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' } },
      { timezone: 'Asia/Tokyo', anchor: 1_799_000_000_000 },
    );
    expect(call).toHaveBeenCalledWith('cron.preview', {
      interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
      timezone: 'Asia/Tokyo',
      anchor: 1_799_000_000_000,
    });
  });
});

describe('WsRpcApi edit + pause faces (A9 — cron.update / cron.pause / cron.resume)', () => {
  const updatedRow = { id: 'cron-w1', cron: '0 9 * * *', prompt: 'edited', recurring: true };

  it('Given an edit with every field, When updateWorkspaceCronTask is called, Then cron.update rides the workspace id + the field patch and the row maps back', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(updatedRow);
    const task = await api.updateWorkspaceCronTask('ws-1', 'cron-w1', {
      title: '每日审查',
      prompt: 'edited',
      cron: '0 9 * * *',
      timezone: 'Asia/Tokyo',
      endsAt: null,
      maxFires: 10,
      model: null,
      effort: 'high',
      permissionMode: 'auto',
    });
    expect(call).toHaveBeenCalledWith('cron.update', {
      workspaceId: 'ws-1',
      taskId: 'cron-w1',
      title: '每日审查',
      prompt: 'edited',
      cron: '0 9 * * *',
      timezone: 'Asia/Tokyo',
      endsAt: null,
      maxFires: 10,
      model: null,
      effort: 'high',
      permissionMode: 'auto',
    });
    expect(task.id).toBe('cron-w1');
  });

  it('Given an untouched field, When the edit is called, Then its key stays absent (the server keeps the current value)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(updatedRow);
    await api.updateWorkspaceCronTask('ws-1', 'cron-w1', { title: 'renamed' });
    const params = call.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(params).toEqual({ workspaceId: 'ws-1', taskId: 'cron-w1', title: 'renamed' });
    // A9: the workspace is the ADDRESS, never an editable field.
    expect(params).not.toHaveProperty('workspace');
  });

  it('Given an empty ack, When the edit is called, Then the adapter rejects (no silent success)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);
    await expect(api.updateWorkspaceCronTask('ws-1', 'cron-w1', { title: 'x' })).rejects.toThrow(
      'did not confirm',
    );
  });

  it('Given pause and resume, When called, Then each rides its own wire method with the workspace + task ids', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ...updatedRow, paused: true });
    const paused = await api.pauseWorkspaceCronTask('ws-1', 'cron-w1');
    expect(call).toHaveBeenCalledWith('cron.pause', { workspaceId: 'ws-1', taskId: 'cron-w1' });
    expect(paused.paused).toBe(true);

    call.mockResolvedValue(updatedRow);
    const resumed = await api.resumeWorkspaceCronTask('ws-1', 'cron-w1');
    expect(call).toHaveBeenCalledWith('cron.resume', { workspaceId: 'ws-1', taskId: 'cron-w1' });
    expect(resumed.paused).toBeUndefined();
  });
});


describe('WsRpcApi task-form fields on the cron rows (form batch 6)', () => {
  it('Given a row carrying the form fields and projections, When mapped, Then every field rides onto AppCronTask', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        {
          id: 'cron-f1', cron: '', prompt: 'sweep', recurring: true,
          stale: false, createdAt: 1,
          title: '双日巡检',
          interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
          endsAt: '2026-12-31',
          model: 'claude-sonnet-4',
          effort: 'low',
          workdir: '/tmp/proj',
          nextTwoFires: [1_700_000_300_000, 1_700_002_030_000],
          lastRunFromRuns: 1_700_000_100_000,
        },
      ],
    });

    const tasks = await api.listCronTasks('s-1');
    const task = tasks[0];
    expect(task?.title).toBe('双日巡检');
    expect(task?.interval).toEqual({ every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' });
    expect(task?.endsAt).toBe('2026-12-31');
    expect(task?.model).toBe('claude-sonnet-4');
    expect(task?.effort).toBe('low');
    expect(task?.workdir).toBe('/tmp/proj');
    expect(task?.nextTwoFires).toEqual([1_700_000_300_000, 1_700_002_030_000]);
    expect(task?.lastRunFromRuns).toBe(1_700_000_100_000);
  });

  it('Given a row without the form fields, When mapped, Then the absent keys read as absent (no invented values)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        { id: 'cron-old', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
          stale: false, createdAt: 1 },
      ],
    });

    const tasks = await api.listCronTasks('s-1');
    expect(tasks[0]?.title).toBeUndefined();
    expect(tasks[0]?.interval).toBeUndefined();
    expect(tasks[0]?.nextTwoFires).toBeUndefined();
    expect(tasks[0]?.lastRunFromRuns).toBeUndefined();
  });

  it('Given an interval task, When created through the workspace face, Then cron stays absent and interval rides the wire', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ taskId: 'cron-i1', cron: '', recurring: true });
    await api.createWorkspaceCronTask('ws-1', {
      prompt: 'sweep', recurring: true,
      interval: { every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' },
      title: '双日巡检', endsAt: '2026-12-31',
      model: 'claude-sonnet-4', effort: 'low', workdir: '/tmp/proj',
    });
    const params = call.mock.calls[0]?.[1] as Record<string, unknown>;
    expect(params['cron']).toBeUndefined();
    expect(params['interval']).toEqual({ every: 2, unit: 'day', time: '09:30', anchorDate: '2026-11-01' });
    expect(params['title']).toBe('双日巡检');
    expect(params['endsAt']).toBe('2026-12-31');
    expect(params['model']).toBe('claude-sonnet-4');
    expect(params['effort']).toBe('low');
    expect(params['workdir']).toBe('/tmp/proj');
  });
});
