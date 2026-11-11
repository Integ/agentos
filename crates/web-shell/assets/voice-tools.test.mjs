import test from 'node:test';
import assert from 'node:assert/strict';
import { VoiceToolRouter, validateVoiceTool } from './voice-tools.js';

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function turns() { for (let i = 0; i < 8; i++) await Promise.resolve(); }
const intent = (id, text = '记一下 一个念头') => ({ callId: id, name: 'agentos_intent', arguments: JSON.stringify({ text }) });
function router(overrides = {}) {
  return new VoiceToolRouter({ performIntent: async text => ({ ok: true, text }),
    showSpace: async scene => ({ ok: true, scene }), controlFocus: async args => ({ ok: true, ...args }), ...overrides });
}

test('only complete allowlisted tool arguments are accepted; text stays inert', () => {
  assert.deepEqual(validateVoiceTool('agentos_intent', { text: '  记一下 <script>不执行</script>  ' }),
    { name: 'agentos_intent', args: { text: '记一下 <script>不执行</script>' } });
  assert.deepEqual(validateVoiceTool('agentos_intent', { text: '好的' }), { name: 'agentos_intent', args: { text: '好的' } });
  for (const space of ['overview', 'focus', 'memory', 'activity', 'clock', 'help']) assert.ok(validateVoiceTool('show_space', { space }));
  for (const action of ['start', 'pause', 'reset']) for (const minutes of [25, 45, 60, null]) assert.ok(validateVoiceTool('focus_timer', { action, minutes }));
  for (const [name, args] of [
    ['approve', { task_id: 1 }], ['cancel', {}], ['raw_call', { method: 'approve' }],
    ['agentos_intent', { text: '' }], ['agentos_intent', { text: '界'.repeat(6000) }],
    ['agentos_intent', { text: '记一下 茶', confirmation: 'opaque' }],
    ['agentos_intent', { text: '记一下 茶', html: '<script/>' }],
    ['show_space', { space: 'payment' }], ['show_space', { space: 'memory', action: 'approve' }],
    ['focus_timer', { action: 'start', minutes: 30 }], ['focus_timer', { action: 'start' }],
    ['focus_timer', { action: 'approve', minutes: null }],
  ]) assert.equal(validateVoiceTool(name, args), null);
  assert.equal(validateVoiceTool('agentos_intent', Object.create({ text: 'hidden' })), null);
  assert.equal(validateVoiceTool('agentos_intent', JSON.parse('{"text":"hi","__proto__":{}}')), null);
});

test('continuous turns are serialized without losing either action', async () => {
  const pending = deferred(), calls = [];
  const tools = router({ performIntent: text => { calls.push(text); return calls.length === 1 ? pending.promise : { ok: true, text }; } });
  const first = tools.execute(intent('first', '记一下 第一条'));
  const second = tools.execute(intent('second', '记一下 第二条'));
  await turns(); assert.deepEqual(calls, ['记一下 第一条']);
  pending.resolve({ ok: true, task_id: 1, status: 'queued' });
  assert.equal((await first).status, 'queued');
  assert.equal((await second).text, '记一下 第二条');
  assert.deepEqual(calls, ['记一下 第一条', '记一下 第二条']);
});

test('a call ID executes once across duplicate events and rejects conflicting arguments', async () => {
  const pending = deferred(); let count = 0;
  const tools = router({ performIntent: () => { count++; return pending.promise; } });
  const first = tools.execute(intent('same'));
  const duplicate = tools.execute({ ...intent('same'), arguments: { text: '记一下 一个念头' } });
  const conflict = await tools.execute(intent('same', '另一件事'));
  assert.equal(conflict.error, 'conflicting_call_id');
  await turns(); assert.equal(count, 1);
  pending.resolve({ ok: true, task: { id: 7, state: 'succeeded' } });
  const result = await first; result.task.state = 'forged';
  assert.equal((await duplicate).task.state, 'succeeded');
  assert.equal((await tools.execute(intent('same'))).task.state, 'succeeded');
  assert.equal(count, 1);
});

test('queued interruption cancels before dispatch but an active OS action keeps its actual result', async () => {
  const pending = deferred(), active = new AbortController(), queued = new AbortController();
  const calls = [];
  const tools = router({ performIntent: text => { calls.push(text); return pending.promise; } });
  const first = tools.execute(intent('active'), { signal: active.signal });
  await turns();
  const second = tools.execute(intent('queued', '不能执行'), { signal: queued.signal });
  queued.abort(); active.abort();
  assert.deepEqual(await second, { ok: false, cancelled: true, error: 'cancelled_before_dispatch' });
  pending.resolve({ ok: true, status: 'succeeded', task_id: 9 });
  assert.equal((await first).status, 'succeeded');
  assert.equal(calls.length, 1);
  assert.equal((await tools.execute(intent('active'))).task_id, 9);
  assert.equal(calls.length, 1);
});

test('unknown mutation outcomes are explicit, redacted, and never automatically retried', async () => {
  let count = 0;
  const tools = router({ performIntent: () => { count++; throw Object.assign(new Error('Bearer secret-value do not echo'), { unknown: true }); } });
  const result = await tools.execute(intent('ambiguous'));
  assert.equal(result.ok, false); assert.equal(result.outcome_unknown, true);
  assert.equal(result.error, 'outcome_unknown');
  assert.ok(!JSON.stringify(result).includes('secret-value'));
  assert.deepEqual(await tools.execute(intent('ambiguous')), result);
  assert.equal(count, 1);
});

test('malformed JSON and unknown tools never reach application callbacks', async () => {
  let count = 0;
  const tools = router({ performIntent: () => { count++; } });
  for (const call of [
    { ...intent('bad'), arguments: '{' }, { ...intent('bad'), arguments: 'x'.repeat(17000) },
    { callId: 'bad', name: 'approve', arguments: '{}' }, { ...intent('bad'), callId: '' },
    { ...intent('bad'), arguments: { text: '记一下', script: 'run()' } },
  ]) assert.equal((await tools.execute(call)).error, 'invalid_tool');
  assert.equal(count, 0);
  assert.deepEqual(await tools.execute({ callId: 'space', name: 'show_space', arguments: '{"space":"memory"}' }), { ok: true, scene: 'memory' });
  assert.deepEqual(await tools.execute({ callId: 'timer', name: 'focus_timer', arguments: { action: 'start', minutes: 45 } }), { ok: true, action: 'start', minutes: 45 });
});

test('results remove credentials recursively and remain bounded valid JSON', async () => {
  const tools = router({ performIntent: () => ({ ok: true, task_id: 4,
    task: { state: 'awaiting_confirmation', confirmation: { token: 'private' }, args: { API_KEY: 'never', password: 'never', text: 'safe' } },
    Authorization: 'Bearer never', text: 'sk-proj-abcdef0123456789 Bearer abcdef0123456789',
  }) });
  const safe = await tools.execute(intent('redact'));
  assert.equal(safe.task.state, 'awaiting_confirmation');
  assert.deepEqual(safe.task.args, { text: 'safe' });
  assert.ok(!JSON.stringify(safe).includes('private'));
  assert.ok(!JSON.stringify(safe).includes('abcdef0123456789'));
  const long = router({ performIntent: () => ({ ok: false, outcome_unknown: true, accepted: true, task_id: 5,
    task: { id: 5, state: 'queued', confirmation: { token: 'never' } },
    notes: Array(30).fill('\u0001'.repeat(1500)), status: 'queued' }) });
  const bounded = await long.execute(intent('huge'));
  assert.ok(new TextEncoder().encode(JSON.stringify(bounded)).byteLength <= 16384);
  assert.equal(bounded.truncated, true); assert.equal(bounded.outcome_unknown, true);
  assert.equal(bounded.task_id, 5); assert.equal(bounded.status, 'queued');
  assert.equal(bounded.accepted, true); assert.deepEqual(bounded.task, { id: 5, state: 'queued' });
});

test('reset invalidates queued old-session calls without cancelling dispatched work', async () => {
  const pending = deferred(), calls = [];
  const tools = router({ performIntent: text => { calls.push(text); return calls.length === 1 ? pending.promise : { ok: true, text }; } });
  const first = tools.execute(intent('old-active', '旧会话已提交'));
  await turns();
  const queued = tools.execute(intent('old-queued', '旧会话未提交'));
  tools.reset();
  assert.equal((await queued).cancelled, true);
  const next = tools.execute(intent('new', '新会话'));
  await turns(); assert.deepEqual(calls, ['旧会话已提交']);
  pending.resolve({ ok: true, status: 'succeeded' });
  assert.equal((await first).status, 'succeeded');
  assert.equal((await next).text, '新会话');
  assert.deepEqual(calls, ['旧会话已提交', '新会话']);
});

test('the bounded ledger refuses new calls rather than evicting IDs that might replay', async () => {
  let count = 0;
  const tools = router({ performIntent: () => ({ ok: true, sequence: ++count }) });
  for (let index = 0; index < 100; index++) assert.equal((await tools.execute(intent(`call-${index}`))).sequence, index + 1);
  assert.equal((await tools.execute(intent('overflow'))).error, 'session_tool_limit');
  assert.equal((await tools.execute(intent('call-0'))).sequence, 1);
  assert.equal(count, 100);
  tools.reset();
  assert.equal((await tools.execute(intent('new-session'))).sequence, 101);
});
