// Renders docs/media/agentos-intro.mp4 and its poster.
//
//   npm install                 # playwright-core (drives a headless Chromium)
//   node capture.mjs            # screenshots of the real UIs
//   .venv/bin/python tts.py     # narration.wav + timeline.json
//   node render.mjs             # → docs/media/agentos-intro.mp4, agentos-intro-poster.jpg
//
//   node render.mjs --stills 5,30,61.5   # only write those frames to out/intro-video/stills/
//   node render.mjs --height 720         # output height (default 1080)
//
// video.html is seeked frame by frame (never played), so the result does not
// depend on how fast this machine renders. Needs ffmpeg on PATH.

import { chromium } from 'playwright-core';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { findChromium } from './capture.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../..');
const work = path.join(root, 'out/intro-video');
const media = path.join(root, 'docs/media');
const FPS = 30;

const args = process.argv.slice(2);
const option = (name) => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);
const height = Number(option('--height') ?? 1080);
const stills = option('--stills')?.split(',').map(Number);
const workers = Number(option('--workers') ?? Math.max(1, Math.min(6, os.cpus().length - 2)));

const timeline = JSON.parse(fs.readFileSync(path.join(work, 'timeline.json'), 'utf8'));
const frames = Math.ceil(timeline.duration * FPS);

const browser = await chromium.launch({ executablePath: findChromium() });
async function openPage() {
  const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: height / 720 });
  const page = await context.newPage();
  page.on('pageerror', (error) => { console.error('page error:', error.message); process.exitCode = 1; });
  await page.goto(pathToFileURL(path.join(here, 'video.html')).href);
  await page.evaluate((tl) => window.setup(tl), timeline);
  return page;
}

function ffmpeg(argv) {
  const child = spawn('ffmpeg', ['-v', 'error', '-y', ...argv], { stdio: ['pipe', 'inherit', 'inherit'] });
  const done = once(child, 'close').then(([code]) => { if (code) throw new Error(`ffmpeg exited with ${code}`); });
  return { child, done };
}

if (stills) {
  const dir = path.join(work, 'stills');
  fs.mkdirSync(dir, { recursive: true });
  const page = await openPage();
  for (const t of stills) {
    await page.evaluate((time) => window.seek(time), t);
    await page.screenshot({ path: path.join(dir, `${t.toFixed(1).padStart(5, '0')}.jpg`), type: 'jpeg', quality: 88 });
  }
  await browser.close();
  console.log(`${stills.length} stills → ${dir}`);
  process.exit();
}

// --- video: each worker renders a contiguous slice into its own segment ---
const started = Date.now();
const per = Math.ceil(frames / workers);
let rendered = 0;
const segments = await Promise.all(Array.from({ length: workers }, async (_, index) => {
  const from = index * per;
  const to = Math.min(frames, from + per);
  const file = path.join(work, `segment-${index}.mp4`);
  if (from >= to) return null;
  const page = await openPage();
  const encoder = ffmpeg(['-f', 'image2pipe', '-c:v', 'mjpeg', '-framerate', String(FPS), '-i', '-',
    '-c:v', 'libx264', '-preset', 'slow', '-crf', '24', '-pix_fmt', 'yuv420p', '-colorspace', 'bt709', '-color_primaries', 'bt709', '-color_trc', 'bt709', '-color_range', 'tv',
    '-vf', 'scale=in_range=pc:out_range=tv:out_color_matrix=bt709', '-g', String(FPS * 4), file]);
  for (let frame = from; frame < to; frame++) {
    await page.evaluate((time) => window.seek(time), frame / FPS);
    const jpeg = await page.screenshot({ type: 'jpeg', quality: 96 });
    if (!encoder.child.stdin.write(jpeg)) await once(encoder.child.stdin, 'drain');
    if (++rendered % 300 === 0) console.log(`${rendered}/${frames} frames, ${((Date.now() - started) / 1000).toFixed(0)}s`);
  }
  encoder.child.stdin.end();
  await encoder.done;
  await page.context().close();
  return file;
}));

// --- poster: the title card without caption, plus a play badge ---
const poster = await openPage();
const title = timeline.scenes[0];
const minutes = Math.floor(timeline.duration / 60);
const seconds = String(Math.round(timeline.duration % 60)).padStart(2, '0');
await poster.evaluate(({ t, label }) => window.poster(t, label), { t: title.lines[1].start - 0.05, label: `${minutes}:${seconds}` });
const posterPng = path.join(work, 'poster.png');
await poster.screenshot({ path: posterPng });
await browser.close();
await ffmpeg(['-i', posterPng, '-vf', 'scale=1280:720:flags=lanczos', '-q:v', '3', path.join(media, 'agentos-intro-poster.jpg')]).done;

// --- join segments and add the narration ---
const list = path.join(work, 'segments.txt');
fs.writeFileSync(list, segments.filter(Boolean).map((file) => `file '${file}'\n`).join(''));
const output = path.join(media, 'agentos-intro.mp4');
await ffmpeg(['-f', 'concat', '-safe', '0', '-i', list, '-i', path.join(work, 'narration.wav'),
  '-c:v', 'copy', '-af', 'loudnorm=I=-16:TP=-1.5:LRA=11,aresample=48000', '-c:a', 'aac', '-b:a', '112k', '-ac', '2',
  '-t', String(frames / FPS), '-movflags', '+faststart', output]).done;
for (const file of segments.filter(Boolean)) fs.rmSync(file);

const size = (fs.statSync(output).size / 1e6).toFixed(1);
console.log(`${output}\n${minutes}:${seconds}, ${height}p, ${size} MB, rendered in ${((Date.now() - started) / 1000).toFixed(0)}s`);
