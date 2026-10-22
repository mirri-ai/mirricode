import { describe, expect, it, vi } from 'vitest';

import { dataUrl, errorHtml, loadingHtml } from './screens';

vi.mock('./server/daemon', () => ({
  serverLogPath: () => '/tmp/mirri-server-desktop.log',
}));

describe('startup screens', () => {
  it('should escape HTML metacharacters in the error message', () => {
    const html = errorHtml('<script>alert(1)</script> & more');
    expect(html).toContain('&lt;script&gt;alert(1)&lt;/script&gt; &amp; more');
    expect(html).not.toContain('<script>');
  });

  it('should render the loading screen with the boot copy', () => {
    const html = loadingHtml();
    expect(html).toContain('<!doctype html><meta charset="utf-8">');
    expect(html).toContain('正在启动 Mirri 本地服务');
    expect(html).toContain('class="spinner"');
  });

  it('should render the error screen with the daemon log path', () => {
    const html = errorHtml('boom');
    expect(html).toContain('无法启动本地服务');
    expect(html).toContain('查看日志：<code>/tmp/mirri-server-desktop.log</code>');
  });

  it('should wrap screen html into an encoded data url', () => {
    expect(dataUrl('<h1>x</h1>')).toBe(
      'data:text/html;charset=utf-8,%3Ch1%3Ex%3C%2Fh1%3E',
    );
  });
});
