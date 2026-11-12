// Screenshots of the two real UIs, used as footage by video.html.
//
//   node capture.mjs            # writes out/intro-video/shots/*.png
//
// The native desktop (crates/shell/ui/index.html) runs in its built-in browser
// demo mode; the web shell (crates/web-shell/assets) is served statically and
// runs in its offline "local" mode. Neither needs a daemon or a model key.

import { chromium } from 'playwright-core';
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../..');
const out = path.join(root, 'out/intro-video/shots');
fs.mkdirSync(out, { recursive: true });

export function findChromium() {
  if (process.env.CHROMIUM) return process.env.CHROMIUM;
  const cache = path.join(process.env.HOME, 'Library/Caches/ms-playwright');
  const hits = [
    ...fs.globSync(path.join(cache, 'chromium_headless_shell-*/*/chrome-headless-shell')),
    ...fs.globSync(path.join(process.env.HOME, '.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell')),
  ];
  if (!hits.length) throw new Error('No headless Chromium found; set CHROMIUM=/path/to/chrome or run `npx playwright install chromium-headless-shell`.');
  return hits.sort().at(-1);
}

function serve(dir) {
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css' };
  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url, 'http://local').pathname;
    const file = path.join(dir, pathname === '/' ? 'index.html' : pathname);
    if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404, { 'content-type': 'application/json' });
      res.end('{}');
      return;
    }
    res.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'application/octet-stream' });
    res.end(fs.readFileSync(file));
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server)));
}

async function main() {
  const browser = await chromium.launch({ executablePath: findChromium() });
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1.5,
    locale: 'zh-CN',
    reducedMotion: 'reduce',
  });
  const shot = (page, name) => page.screenshot({ path: path.join(out, `${name}.png`) });
  const say = async (page, selector, text, wait = 1600) => {
    await page.fill(selector, text);
    await page.press(selector, 'Enter');
    await page.waitForTimeout(wait);
  };

  // --- native desktop: agentos-shell in demo mode ---
  const shellUrl = 'file://' + path.join(root, 'crates/shell/ui/index.html');
  const desk = await context.newPage();
  await desk.goto(shellUrl);
  await desk.waitForTimeout(800);
  await say(desk, '#intent', '记一下 周五前提交报销单');
  await say(desk, '#intent', '看看笔记');
  await desk.mouse.move(720, 700);
  await shot(desk, 'desktop');
  await say(desk, '#intent', '清空笔记');
  await shot(desk, 'confirm');
  await desk.close();

  const setup = await context.newPage();
  await setup.goto(shellUrl + '#setup');
  await setup.waitForTimeout(1200);
  await shot(setup, 'setup');
  await setup.close();

  // --- web shell: offline local mode ---
  const server = await serve(path.join(root, 'crates/web-shell/assets'));
  const web = await context.newPage();
  await web.goto(`http://127.0.0.1:${server.address().port}/`);
  await web.waitForTimeout(2000);
  await shot(web, 'web-now');
  await say(web, '#intent-input', '记一下 周五前提交报销单', 2000);
  await say(web, '#intent-input', '记一下 给物业打电话问暖气', 2000);
  await shot(web, 'web-memory');
  await say(web, '#intent-input', '开始专注', 2000);
  await shot(web, 'web-focus');
  await web.close();
  server.close();

  await browser.close();
  console.log('shots →', out);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
