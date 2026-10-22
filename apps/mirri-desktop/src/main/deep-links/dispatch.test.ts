import { describe, expect, it, vi } from 'vitest';

import { IPC_CHANNELS } from '../ipc';
import { createDeepLinkDispatch } from './dispatch';

/** Minimal BrowserWindow stand-in: records main→renderer sends and the
 *  isDestroyed() state the dispatch gates on. */
function makeWin(overrides: Partial<{ isDestroyed: boolean }> = {}) {
  const send = vi.fn();
  const win = {
    isDestroyed: () => overrides.isDestroyed ?? false,
    webContents: { send },
  };
  return { win, send };
}

function makeDispatch() {
  const showMainWindow = vi.fn();
  let current: ReturnType<typeof makeWin> | null = null;
  const dispatch = createDeepLinkDispatch({
    getMainWindow: () => (current === null ? null : (current.win as never)),
    showMainWindow,
  });
  return {
    dispatch,
    showMainWindow,
    setWin: (w: ReturnType<typeof makeWin> | null) => (current = w),
  };
}

describe('createDeepLinkDispatch', () => {
  describe('handleDeepLink', () => {
    it('should forward a valid link immediately when a live window exists and the renderer is ready', () => {
      const { dispatch, showMainWindow, setWin } = makeDispatch();
      const win = makeWin();
      setWin(win);
      dispatch.onRendererReady();

      dispatch.handleDeepLink('mirricode://session/abc');

      expect(showMainWindow).toHaveBeenCalledTimes(1);
      expect(win.send).toHaveBeenCalledWith(
        IPC_CHANNELS.deepLink,
        'mirricode://session/abc',
      );
    });

    it('should queue a valid link and still show the window when the renderer is not ready yet', () => {
      const { dispatch, showMainWindow, setWin } = makeDispatch();
      const win = makeWin();
      setWin(win);

      dispatch.handleDeepLink('mirricode://session/abc');

      expect(showMainWindow).toHaveBeenCalledTimes(1);
      expect(win.send).not.toHaveBeenCalled();
    });

    it('should queue a valid link when no main window exists yet', () => {
      const { dispatch, showMainWindow } = makeDispatch();

      dispatch.handleDeepLink('mirricode://session/abc');

      // No window yet: nothing to send to, but the shell still brings the
      // app to the front (showMainWindow creates/focuses the window).
      expect(showMainWindow).toHaveBeenCalledTimes(1);
    });

    it('should queue a valid link when the main window is destroyed', () => {
      const { dispatch, setWin } = makeDispatch();
      const win = makeWin({ isDestroyed: true });
      setWin(win);

      dispatch.handleDeepLink('mirricode://session/abc');

      expect(win.send).not.toHaveBeenCalled();
    });

    it('should drop a non-mirricode url without queuing or showing the window', () => {
      const { dispatch, showMainWindow, setWin } = makeDispatch();
      const win = makeWin();
      setWin(win);

      dispatch.handleDeepLink('https://example.com');

      expect(showMainWindow).not.toHaveBeenCalled();
      expect(win.send).not.toHaveBeenCalled();
    });
  });

  describe('onRendererReady', () => {
    it('should flush queued links to the main window once the renderer reports ready', () => {
      const { dispatch, setWin } = makeDispatch();
      dispatch.handleDeepLink('mirricode://session/abc');
      const win = makeWin();
      setWin(win);

      dispatch.onRendererReady();

      expect(win.send).toHaveBeenCalledWith(
        IPC_CHANNELS.deepLink,
        'mirricode://session/abc',
      );
    });

    it('should not resend links that were already delivered', () => {
      const { dispatch, setWin } = makeDispatch();
      const win = makeWin();
      setWin(win);
      dispatch.onRendererReady();

      dispatch.handleDeepLink('mirricode://session/abc');
      expect(win.send).toHaveBeenCalledTimes(1);

      // A reload re-fires did-finish-load; the queue is empty by then.
      dispatch.onRendererReady();

      expect(win.send).toHaveBeenCalledTimes(1);
    });

    it('should drop links whose flush finds no live window instead of leaking them into the next window', () => {
      const { dispatch, setWin } = makeDispatch();
      dispatch.handleDeepLink('mirricode://session/abc');
      // Flush fires while no window exists — the queued link is dropped
      // (flushPendingDeepLinks splices the queue, then only sends to a
      // live window).
      dispatch.onRendererReady();

      const win = makeWin();
      setWin(win);
      dispatch.handleDeepLink('mirricode://open');

      expect(win.send).toHaveBeenCalledTimes(1);
      expect(win.send).toHaveBeenCalledWith(IPC_CHANNELS.deepLink, 'mirricode://open');
    });
  });

  describe('onWindowClosed', () => {
    it('should clear the ready flag so links after a window recreation queue again', () => {
      const { dispatch, setWin } = makeDispatch();
      const win1 = makeWin();
      setWin(win1);
      dispatch.onRendererReady();

      const win2 = makeWin();
      setWin(win2);
      dispatch.onWindowClosed();

      dispatch.handleDeepLink('mirricode://session/abc');

      expect(win2.send).not.toHaveBeenCalled();
    });
  });
});
