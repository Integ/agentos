import test from 'node:test';
import assert from 'node:assert/strict';
import { LayoutCoordinator } from './layout-client.js';

class Clock {
  time = 0;
  next = 1;
  timers = new Map();
  now = () => this.time;
  setTimer = (callback, delay) => {
    const id = this.next++;
    this.timers.set(id, { at: this.time + delay, callback });
    return id;
  };
  clearTimer = id => this.timers.delete(id);
  async advance(milliseconds = 0) {
    const end = this.time + milliseconds;
    for (;;) {
      const next = [...this.timers].filter(([, value]) => value.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      this.time = next[1].at;
      this.timers.delete(next[0]);
      next[1].callback();
      await turns();
    }
    this.time = end;
    await turns();
  }
}

async function turns() {
  for (let i = 0; i < 4; i++) await Promise.resolve();
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function response(title) { return { source: 'model', scene: { title, panels: [] } }; }
function setup(overrides = {}) {
  const clock = new Clock(), calls = [], scenes = [], statuses = [];
  const coordinator = new LayoutCoordinator({
    now: clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer,
    request: (context, options) => {
      const result = deferred(); calls.push({ context, ...options, ...result }); return result.promise;
    },
    onScene: (value, key) => scenes.push({ value, key }),
    onStatus: status => statuses.push(status), ...overrides,
  });
  return { clock, calls, scenes, statuses, coordinator };
}

test('debounces changes and settles an unchanged context without polling the model', async () => {
  const { coordinator, clock, calls, scenes, statuses } = setup();
  assert.equal(coordinator.update('a', { page: 'a' }), true);
  await clock.advance(449);
  assert.equal(calls.length, 0);
  coordinator.update('b', { page: 'b' });
  await clock.advance(449);
  assert.equal(calls.length, 0);
  await clock.advance(1);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].context, { page: 'b' });
  calls[0].resolve(response('B'));
  await turns();
  assert.equal(scenes[0].key, 'b');
  assert.equal(statuses.at(-1).state, 'ready');
  assert.equal(coordinator.update('b', { ignored: 'same key' }), false);
  await clock.advance(999999);
  assert.equal(calls.length, 1);
});

test('abort plus generation version prevents a late old request from replacing a newer scene', async () => {
  const { coordinator, clock, calls, scenes, statuses } = setup({ debounceMs: 0 });
  coordinator.update('old', { selected: 1 }); await clock.advance();
  coordinator.update('new', { selected: 2 }); await clock.advance();
  assert.equal(calls[0].signal.aborted, true);
  calls[1].resolve(response('latest')); await turns();
  calls[0].resolve(response('late obsolete response')); await turns();
  assert.deepEqual(scenes.map(item => item.value.scene.title), ['latest']);
  assert.equal(statuses.at(-1).key, 'new');
  assert.equal(statuses.at(-1).state, 'ready');
});

test('a failed key remains in fallback until an explicit retry, without automatic replays', async () => {
  const { coordinator, clock, calls, scenes, statuses } = setup({ debounceMs: 0 });
  coordinator.update('same', {}); await clock.advance();
  calls[0].reject(new Error('provider unavailable')); await turns();
  assert.equal(statuses.at(-1).state, 'fallback');
  assert.equal(statuses.at(-1).error.message, 'provider unavailable');
  assert.equal(coordinator.update('same', {}), false);
  await clock.advance(300000);
  assert.equal(calls.length, 1);
  assert.equal(coordinator.retry(), true); await clock.advance();
  calls[1].resolve(response('retried')); await turns();
  assert.equal(scenes.at(-1).value.scene.title, 'retried');
});

test('uses only exact-key cached plans and bounds them by LRU and expiry', async () => {
  const { coordinator, clock, calls, scenes } = setup({ debounceMs: 0, cacheSize: 2, cacheTtlMs: 100 });
  async function generate(key) {
    coordinator.update(key, { key }); await clock.advance();
    calls.at(-1).resolve(response(key)); await turns();
  }
  await generate('a'); await generate('b');
  coordinator.update('a', { key: 'a' });
  assert.equal(calls.length, 2);
  assert.equal(scenes.at(-1).key, 'a');
  await generate('c');
  coordinator.update('b', { key: 'b' }); await clock.advance();
  assert.equal(calls.length, 4, 'b was least recently used and was evicted');
  calls.at(-1).resolve(response('b again')); await turns();
  await clock.advance(101);
  coordinator.update('c', { key: 'c' }); await clock.advance();
  assert.equal(calls.length, 5, 'c expired instead of being reused');
  calls.at(-1).resolve(response('c again')); await turns();
});

test('onScene can re-render and call update with the same key without starting a request loop', async () => {
  const clock = new Clock(), calls = [];
  let coordinator, deliveries = 0;
  coordinator = new LayoutCoordinator({ debounceMs: 0, now: clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer,
    request: async context => { calls.push(context); return response(context.title); },
    onScene: (value, key) => { deliveries++; assert.equal(coordinator.update(key, { title: value.scene.title }), false); },
  });
  coordinator.update('stable', { title: 'one render' }); await clock.advance();
  assert.equal(deliveries, 1); assert.equal(calls.length, 1);
  coordinator.update('other', { title: 'other render' }); await clock.advance();
  coordinator.update('stable', { title: 'one render' });
  assert.equal(deliveries, 3); assert.equal(calls.length, 2);
});

test('reset and dispose invalidate pending work even if the request ignores abort', async () => {
  const { coordinator, clock, calls, scenes, statuses } = setup({ debounceMs: 0 });
  coordinator.update('a', {}); await clock.advance();
  coordinator.reset();
  assert.equal(calls[0].signal.aborted, true);
  calls[0].resolve(response('after reset')); await turns();
  assert.equal(scenes.length, 0); assert.equal(statuses.at(-1).state, 'idle');
  assert.equal(coordinator.retry(), false);
  coordinator.update('b', {}); await clock.advance();
  const beforeDispose = statuses.length;
  coordinator.dispose();
  calls[1].resolve(response('after dispose')); await turns();
  assert.equal(scenes.length, 0); assert.equal(statuses.length, beforeDispose);
  assert.equal(coordinator.update('c', {}), false); assert.equal(coordinator.retry(), false);
  assert.equal(clock.timers.size, 0);
});

test('cached responses are detached from renderer mutation and retry replaces a successful cache', async () => {
  const { coordinator, clock, calls, scenes } = setup({ debounceMs: 0 });
  coordinator.update('a', {}); await clock.advance(); calls[0].resolve(response('original')); await turns();
  scenes[0].value.scene.title = 'renderer changed its copy';
  coordinator.update('b', {}); await clock.advance(); calls[1].resolve(response('B')); await turns();
  coordinator.update('a', {});
  assert.equal(scenes.at(-1).value.scene.title, 'original');
  coordinator.retry(); await clock.advance();
  assert.equal(calls.length, 3);
  calls[2].resolve(response('new version')); await turns();
  assert.equal(scenes.at(-1).value.scene.title, 'new version');
});

test('a status hook changing context cannot leave a stale debounce timer behind', async () => {
  const clock = new Clock(), calls = [];
  let coordinator;
  coordinator = new LayoutCoordinator({ debounceMs: 0, now: clock.now, setTimer: clock.setTimer, clearTimer: clock.clearTimer,
    request: async context => { calls.push(context); return response('current'); },
    onStatus: status => { if (status.state === 'generating' && status.key === 'a') coordinator.update('b', { key: 'b' }); },
  });
  coordinator.update('a', { key: 'a' }); await clock.advance();
  assert.deepEqual(calls, [{ key: 'b' }]);
});
