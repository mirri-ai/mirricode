// apps/mirri-desktop/src/main/screens.ts
//
// Data-URL HTML shown inside the BrowserWindow while the daemon boots
// (loading) or when it failed to start (error). Pure string builders —
// no Electron imports beyond nothing; fully unit-testable.

import { serverLogPath } from './server/daemon';

export function dataUrl(html: string): string {
  return `data:text/html;charset=utf-8,${encodeURIComponent(html)}`;
}

const SCREEN_STYLE = `
  <style>
    html, body { height: 100%; margin: 0; }
    body {
      display: flex; flex-direction: column; align-items: center; justify-content: center;
      gap: 18px; background: #0b0b0c; color: #e7e7ea; font: 14px/1.5 system-ui, sans-serif;
      -webkit-user-select: none; user-select: none; text-align: center; padding: 0 32px;
    }
    .spinner {
      width: 34px; height: 34px; border-radius: 50%;
      border: 3px solid #2a2a2e; border-top-color: #7c8cff; animation: spin 0.9s linear infinite;
    }
    @keyframes spin { to { transform: rotate(360deg); } }
    h1 { font-size: 15px; font-weight: 600; margin: 0; }
    p { margin: 0; color: #9a9aa2; max-width: 560px; }
    code { color: #c8c8d0; word-break: break-all; }
  </style>
`;

export function loadingHtml(): string {
  return `<!doctype html><meta charset="utf-8">${SCREEN_STYLE}
    <div class="spinner"></div>
    <h1>正在启动 Mirri 本地服务…</h1>
    <p>首次启动可能需要几秒。</p>`;
}

export function errorHtml(message: string): string {
  const safe = message.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  return `<!doctype html><meta charset="utf-8">${SCREEN_STYLE}
    <h1>无法启动本地服务</h1>
    <p>${safe}</p>
    <p>查看日志：<code>${serverLogPath()}</code></p>
    <p>菜单 → Mirri Code Desktop → 重试连接，或先检查日志。</p>`;
}
