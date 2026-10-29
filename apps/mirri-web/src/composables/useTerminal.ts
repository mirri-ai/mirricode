import { getCurrentInstance, onUnmounted, ref, watch, type Ref } from 'vue';
import { getMirriWebApi } from '../api';
import type { AppTerminal, MirriEventConnection, TerminalEventSinks } from '../api/types';

export function useTerminal(sessionId: Ref<string>) {
  const terminal = ref<AppTerminal | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);
  const connected = ref(false);
  const readOnly = ref(false);
  const lastSeq = ref(0);

  const outputHandlers = new Set<(data: string) => void>();
  const exitHandlers = new Set<(exitCode: number | null) => void>();
  let conn: MirriEventConnection | null = null;
  // Set on unmount so an in-flight start() (its awaits resolve after the
  // panel is gone) can never bind a fresh connection afterwards.
  let disposed = false;

  const sinks: TerminalEventSinks = {
    onTerminalOutput: (sid, terminalId, data, seq) => {
      if (sid !== sessionId.value || terminal.value?.id !== terminalId) return;
      lastSeq.value = Math.max(lastSeq.value, seq);
      for (const handler of outputHandlers) handler(data);
    },
    onTerminalExit: (sid, terminalId, exitCode) => {
      if (sid !== sessionId.value || terminal.value?.id !== terminalId) return;
      readOnly.value = true;
      terminal.value = terminal.value
        ? { ...terminal.value, status: 'exited', exitCode }
        : terminal.value;
      for (const handler of exitHandlers) handler(exitCode);
    },
    onError: (_code, msg) => {
      error.value = msg;
    },
    onConnectionChange: (state) => {
      connected.value = state;
    },
  };

  function ensureConnection(): MirriEventConnection | null {
    if (conn !== null || disposed) return conn;
    if (typeof WebSocket === 'undefined') return null;
    const api = getMirriWebApi();
    // The rust backend's event connection is a singleton whose connectEvents()
    // REPLACES the app's bound handler set — binding a private handler set
    // here would hijack the app's live event stream (and this connection's
    // close() would tear down the shared transport). connectTerminalEvents()
    // binds the terminal sinks additively instead; backends without it (the
    // v1/v2 TS daemon) hand out a private connection per connectEvents()
    // call, which is safe to own.
    if (api.connectTerminalEvents) {
      conn = api.connectTerminalEvents(sinks);
    } else {
      conn = api.connectEvents({
        onEvent: () => {},
        onResync: () => {},
        onError: (code, msg, fatal) => sinks.onError?.(code, msg, fatal),
        onConnectionChange: (state) => sinks.onConnectionChange?.(state),
        onTerminalOutput: (sid, terminalId, data, seq) => sinks.onTerminalOutput(sid, terminalId, data, seq),
        onTerminalExit: (sid, terminalId, exitCode) => sinks.onTerminalExit(sid, terminalId, exitCode),
      });
    }
    return conn;
  }

  async function start(size?: { cols?: number; rows?: number }): Promise<void> {
    const sid = sessionId.value;
    if (!sid || loading.value) return;
    loading.value = true;
    error.value = null;
    try {
      const api = getMirriWebApi();
      const existing = (await api.listTerminals(sid)).find((item) => item.status === 'running');
      const next = existing ?? await api.createTerminal(sid, {
        cols: size?.cols,
        rows: size?.rows,
      });
      if (disposed || sessionId.value !== sid) return;
      terminal.value = next;
      readOnly.value = next.status === 'exited';
      ensureConnection()?.terminalAttach(sid, next.id, lastSeq.value);
    } catch (error_) {
      error.value = error_ instanceof Error ? error_.message : String(error_);
    } finally {
      loading.value = false;
    }
  }

  function write(data: string): void {
    const current = terminal.value;
    if (!current || readOnly.value) return;
    ensureConnection()?.terminalInput(current.sessionId, current.id, data);
  }

  function resize(cols: number, rows: number): void {
    const current = terminal.value;
    if (!current || readOnly.value) return;
    ensureConnection()?.terminalResize(current.sessionId, current.id, cols, rows);
  }

  async function close(): Promise<void> {
    const current = terminal.value;
    if (!current) return;
    readOnly.value = true;
    try {
      ensureConnection()?.terminalClose(current.sessionId, current.id);
      await getMirriWebApi().closeTerminal(current.sessionId, current.id);
    } catch (error_) {
      error.value = error_ instanceof Error ? error_.message : String(error_);
    }
  }

  /** Replace the current terminal with a FRESH one: the old shell is killed
   *  (a plain detach would leave it running, and the next start() would just
   *  re-attach to it — replaying the same wedged shell instead of a new one). */
  async function restart(): Promise<void> {
    const current = terminal.value;
    terminal.value = null;
    readOnly.value = false;
    lastSeq.value = 0;
    if (current) {
      conn?.terminalDetach(current.sessionId, current.id);
      // The kill goes out directly — close() reads terminal.value, which is
      // already nulled above.
      try {
        ensureConnection()?.terminalClose(current.sessionId, current.id);
        await getMirriWebApi().closeTerminal(current.sessionId, current.id);
      } catch {
        // Best-effort degrade: the kill failed, so the old shell SURVIVES and
        // the start below re-attaches it (the pre-"New" behavior). The panel
        // stays functional — surfacing an error overlay over the re-attached
        // shell would mislead, so the degrade stays silent here.
      }
    }
    if (disposed) return;
    await start();
  }

  function onOutput(handler: (data: string) => void): () => void {
    outputHandlers.add(handler);
    return () => outputHandlers.delete(handler);
  }

  function onExit(handler: (exitCode: number | null) => void): () => void {
    exitHandlers.add(handler);
    return () => exitHandlers.delete(handler);
  }

  function release(): void {
    const current = terminal.value;
    if (current) conn?.terminalDetach(current.sessionId, current.id);
    conn?.close();
    conn = null;
  }

  watch(sessionId, () => {
    release();
    terminal.value = null;
    readOnly.value = false;
    lastSeq.value = 0;
  });

  // Register only inside a component setup(); plain unit tests call this
  // composable outside any instance, where lifecycle hooks cannot bind.
  if (getCurrentInstance()) {
    onUnmounted(() => {
      disposed = true;
      release();
    });
  }

  return {
    terminal,
    loading,
    error,
    connected,
    readOnly,
    start,
    write,
    resize,
    close,
    restart,
    onOutput,
    onExit,
  };
}
