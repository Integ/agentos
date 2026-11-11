import test from 'node:test';
import assert from 'node:assert/strict';
import { RealtimeVoiceSession } from './realtime-voice.js';

class Target {
  listeners = new Map();
  addEventListener(type, fn) { if (!this.listeners.has(type)) this.listeners.set(type, new Set()); this.listeners.get(type).add(fn); }
  removeEventListener(type, fn) { this.listeners.get(type)?.delete(fn); }
  emit(type, data = {}) { for (const fn of [...(this.listeners.get(type) || [])]) fn(data); }
}
class Track extends Target {
  enabled = true; stopped = 0;
  stop() { this.stopped++; }
}
class Channel extends Target {
  readyState = 'connecting'; sent = []; closed = false;
  send(data) { if (this.readyState !== 'open') throw new Error('closed'); this.sent.push(JSON.parse(data)); }
  open() { this.readyState = 'open'; this.emit('open'); }
  event(data) { this.emit('message', { data: JSON.stringify(data) }); }
  close() { this.closed = true; this.readyState = 'closed'; this.emit('close'); }
}
class Peer extends Target {
  connectionState = 'new'; iceGatheringState = 'complete'; channel = new Channel(); tracks = []; closed = false;
  addTrack(track, stream) { this.tracks.push({ track, stream }); }
  createDataChannel(label) { assert.equal(label, 'oai-events'); return this.channel; }
  async createOffer() { return { type: 'offer', sdp: 'v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n' }; }
  async setLocalDescription(value) { this.localDescription = value; }
  async setRemoteDescription(value) { this.remoteDescription = value; }
  close() { this.closed = true; this.connectionState = 'closed'; this.emit('connectionstatechange'); }
}
class Audio {
  srcObject = null; plays = 0; pauses = 0; removed = false;
  play() { this.plays++; return Promise.resolve(); }
  pause() { this.pauses++; }
  remove() { this.removed = true; }
}
class Clock {
  time = 0; next = 0; timers = new Map();
  setTimer = (fn, delay) => { const id = ++this.next; this.timers.set(id, { fn, at: this.time + delay }); return id; };
  clearTimer = id => this.timers.delete(id);
  async advance(ms) {
    const end = this.time + ms;
    for (;;) {
      const item = [...this.timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!item) break;
      this.time = item[1].at; this.timers.delete(item[0]); item[1].fn(); await turns();
    }
    this.time = end; await turns();
  }
}
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
async function turns() { for (let i = 0; i < 12; i++) await Promise.resolve(); }
async function until(predicate) { for (let i = 0; i < 200 && !predicate(); i++) await Promise.resolve(); assert.ok(predicate(), 'asynchronous stage should settle'); }
function setup(overrides = {}) {
  const clock = new Clock(), track = new Track(), remoteTrack = new Track(), peer = new Peer(), audio = new Audio();
  const stream = { getTracks: () => [track] }, remote = { getTracks: () => [remoteTrack] };
  const states = [], captions = [], creates = [], releases = [], tools = [];
  let mediaCalls = 0, peerCalls = 0;
  const voice = new RealtimeVoiceSession({
    createSession: async options => { creates.push(options); return { sdp: 'v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n', call_id: 'rtc_test' }; },
    releaseSession: async ({ callId }) => { releases.push(callId); },
    getUserMedia: async options => { mediaCalls++; assert.equal(options.audio.echoCancellation, true); return stream; },
    peerFactory: () => { peerCalls++; return peer; }, audioFactory: () => audio,
    onState: value => states.push(value), onTranscript: value => captions.push(value),
    onToolCall: async call => { tools.push(call); return { ok: true, state: 'succeeded' }; },
    getContext: () => '当前空间：专注', setTimer: clock.setTimer, clearTimer: clock.clearTimer,
    ...overrides,
  });
  return { voice, clock, track, remoteTrack, stream, remote, peer, audio, states, captions, creates, releases, tools,
    mediaCalls: () => mediaCalls, peerCalls: () => peerCalls };
}
async function connect(env) {
  const starting = env.voice.start(); await until(() => env.peer.remoteDescription);
  env.peer.channel.open(); env.peer.channel.event({ type: 'session.created', session: { id: 'session_test' } });
  await starting; return env;
}
const created = id => ({ type: 'response.created', response: { id } });
const done = (id, output = [], status = 'completed') => ({ type: 'response.done', response: { id, status, output } });
const tool = (callId = 'call_1', responseId = 'resp_1', args = '{"text":"记一下 买牛奶"}') => ({ type: 'response.function_call_arguments.done', call_id: callId, response_id: responseId, name: 'agentos_intent', arguments: args });

test('two speech turns stay in one session and transcription never manually commits or invokes tools', async () => {
  const env = await connect(setup());
  assert.equal(env.voice.snapshot.phase, 'listening'); assert.equal(env.voice.snapshot.active, true);
  for (const id of ['input_1', 'input_2']) {
    env.peer.channel.event({ type: 'input_audio_buffer.speech_started', item_id: id });
    assert.equal(env.voice.snapshot.userSpeaking, true);
    env.peer.channel.event({ type: 'conversation.item.input_audio_transcription.delta', item_id: id, delta: '你好' });
    env.peer.channel.event({ type: 'conversation.item.input_audio_transcription.delta', item_id: id, delta: '，继续说' });
    env.peer.channel.event({ type: 'input_audio_buffer.speech_stopped', item_id: id });
    env.peer.channel.event({ type: 'conversation.item.input_audio_transcription.completed', item_id: id, transcript: '你好，继续说。' });
    env.peer.channel.event({ type: 'conversation.item.input_audio_transcription.completed', item_id: id, transcript: '重复' });
  }
  assert.equal(env.mediaCalls(), 1); assert.equal(env.peerCalls(), 1); assert.equal(env.creates.length, 1);
  assert.deepEqual(env.captions.filter(c => c.final).map(c => c.text), ['你好，继续说。', '你好，继续说。']);
  assert.equal(env.captions[1].text, '你好，继续说'); assert.equal(env.tools.length, 0);
  assert.equal(env.peer.channel.sent.filter(e => ['input_audio_buffer.commit', 'response.create'].includes(e.type)).length, 0);
  await env.voice.stop();
});

test('barge-in pauses output, marks caption interrupted and ignores old response events without stopping the mic', async () => {
  const env = await connect(setup());
  env.peer.emit('track', { streams: [env.remote] });
  env.peer.channel.event(created('resp_old'));
  env.peer.channel.event({ type: 'response.output_audio_transcript.delta', response_id: 'resp_old', item_id: 'out_old', delta: '我正在说明' });
  env.peer.channel.event({ type: 'output_audio_buffer.started', response_id: 'resp_old' });
  assert.equal(env.voice.snapshot.assistantSpeaking, true);
  env.peer.channel.event({ type: 'input_audio_buffer.speech_started', item_id: 'new_input' });
  assert.equal(env.voice.snapshot.userSpeaking, true); assert.equal(env.voice.snapshot.assistantSpeaking, false);
  assert.ok(env.captions.at(-1).interrupted); assert.equal(env.track.stopped, 0);
  const count = env.captions.length;
  env.peer.channel.event({ type: 'response.output_audio_transcript.done', response_id: 'resp_old', item_id: 'out_old', transcript: '迟到的旧回答' });
  env.peer.channel.event({ type: 'input_audio_buffer.speech_stopped', item_id: 'new_input' });
  env.peer.channel.event(created('resp_new'));
  env.peer.channel.event({ type: 'output_audio_buffer.started', response_id: 'resp_new' });
  env.peer.channel.event(done('resp_old', [], 'cancelled'));
  assert.equal(env.captions.length, count); assert.equal(env.voice.snapshot.phase, 'speaking');
  assert.equal(env.peer.channel.sent.some(e => e.type === 'conversation.item.truncate'), false);
  await env.voice.stop();
});

test('a completed answer is not retroactively marked interrupted on the next ordinary turn', async () => {
  const env = await connect(setup());
  env.peer.channel.event(created('resp_done'));
  env.peer.channel.event({ type: 'response.output_audio_transcript.done', response_id: 'resp_done', item_id: 'answer', transcript: '完成了。' });
  env.peer.channel.event({ type: 'output_audio_buffer.started', response_id: 'resp_done' });
  env.peer.channel.event(done('resp_done'));
  env.peer.channel.event({ type: 'output_audio_buffer.stopped', response_id: 'resp_done' });
  env.peer.channel.event({ type: 'input_audio_buffer.speech_started', item_id: 'new_input' });
  assert.equal(env.captions.length, 1); assert.equal(env.captions[0].interrupted, false);
  await env.voice.stop();
});

test('manual interruption cancels and clears only speech while keeping the continuous microphone session', async () => {
  const env = await connect(setup());
  env.peer.channel.event(created('resp_1')); env.peer.channel.event({ type: 'output_audio_buffer.started', response_id: 'resp_1' });
  assert.equal(env.voice.interrupt(), true);
  assert.deepEqual(env.peer.channel.sent.filter(e => e.type === 'response.cancel'), [{ type: 'response.cancel', response_id: 'resp_1' }]);
  assert.equal(env.peer.channel.sent.filter(e => e.type === 'output_audio_buffer.clear').length, 1);
  assert.equal(env.voice.snapshot.phase, 'listening'); assert.equal(env.track.stopped, 0); assert.equal(env.peer.closed, false);
  env.peer.channel.event({ type: 'error', error: { code: 'response_cancel_not_active' } });
  assert.equal(env.voice.snapshot.active, true);
  await env.voice.stop();
});

test('completed function calls execute exactly once and continuation waits for response.done', async () => {
  const result = deferred(), calls = [];
  const env = await connect(setup({ onToolCall: async (call, options) => { calls.push({ call, options }); return result.promise; } }));
  env.peer.channel.event(created('resp_1')); env.peer.channel.event(tool()); env.peer.channel.event(tool()); await turns();
  assert.equal(calls.length, 1); assert.deepEqual(calls[0].call.arguments, { text: '记一下 买牛奶' });
  result.resolve({ ok: true, state: 'succeeded' }); await turns();
  assert.equal(env.peer.channel.sent.filter(e => e.type === 'response.create').length, 0);
  env.peer.channel.event(done('resp_1', [{ type: 'function_call', call_id: 'call_1', name: 'agentos_intent', arguments: '{"text":"记一下 买牛奶"}' }])); await turns();
  assert.equal(calls.length, 1);
  const outputs = env.peer.channel.sent.filter(e => e.item?.type === 'function_call_output');
  assert.equal(outputs.length, 1); assert.equal(JSON.parse(outputs[0].item.output).state, 'succeeded');
  assert.equal(env.peer.channel.sent.filter(e => e.type === 'response.create').length, 1);
  env.peer.channel.event(done('resp_1')); assert.equal(env.peer.channel.sent.filter(e => e.type === 'response.create').length, 1);
  await env.voice.stop();
});

test('tools serialize, interruption skips queued actions and already dispatched results do not speak over a new turn', async () => {
  const first = deferred(), calls = [];
  const env = await connect(setup({ onToolCall: async call => { calls.push(call); return first.promise; } }));
  env.peer.channel.event(created('resp_1')); env.peer.channel.event(tool('call_1')); env.peer.channel.event(tool('call_2')); await turns();
  assert.equal(calls.length, 1);
  env.peer.channel.event(done('resp_1'));
  env.peer.channel.event({ type: 'input_audio_buffer.speech_started', item_id: 'correction' });
  first.resolve({ ok: true, task_id: 7, state: 'queued' }); await turns();
  assert.equal(calls.length, 1);
  const outputs = env.peer.channel.sent.filter(e => e.item?.type === 'function_call_output');
  assert.equal(outputs.length, 2); assert.equal(JSON.parse(outputs[1].item.output).executed, false);
  assert.equal(env.peer.channel.sent.filter(e => e.type === 'response.create').length, 0);
  await env.voice.stop();
});

test('stop during a pending microphone permission request settles startup and closes late capture', async () => {
  const media = deferred(); const env = setup({ getUserMedia: () => media.promise });
  const startup = env.voice.start(); await turns(); await env.voice.stop();
  assert.equal((await startup).phase, 'idle'); assert.equal(env.peerCalls(), 0);
  media.resolve(env.stream); await turns();
  assert.equal(env.track.stopped, 1); assert.equal(env.voice.snapshot.active, false); assert.equal(env.creates.length, 0);
});

test('stop aborts a pending SDP request, closes all resources and releases a late server-created call once', async () => {
  const answer = deferred(), creates = [];
  const env = setup({ createSession: options => { creates.push(options); return answer.promise; } });
  const startup = env.voice.start(); await until(() => creates.length);
  assert.equal(creates.length, 1); const stopping = env.voice.stop();
  assert.equal(creates[0].signal.aborted, true); assert.equal(env.track.stopped, 1); assert.equal(env.peer.closed, true); assert.equal(env.audio.srcObject, null);
  answer.resolve({ sdp: 'v=0\r\n', call_id: 'rtc_late' }); await startup; await stopping; await turns();
  assert.deepEqual(env.releases, ['rtc_late']); assert.equal(env.voice.snapshot.phase, 'idle');
});

test('connection timeout and network failure clean up without automatically reopening capture', async () => {
  const env = setup({ handshakeTimeoutMs: 100 });
  const startup = env.voice.start(); const rejected = assert.rejects(startup, /连接超时/); await turns();
  await env.clock.advance(100); await rejected;
  assert.equal(env.track.stopped, 1); assert.equal(env.peer.closed, true); assert.equal(env.voice.snapshot.phase, 'error');
  await env.clock.advance(1000000); assert.equal(env.mediaCalls(), 1);
  const connected = await connect(setup());
  connected.peer.connectionState = 'disconnected'; connected.peer.emit('connectionstatechange');
  assert.equal(connected.voice.snapshot.phase, 'error'); assert.equal(connected.track.stopped, 1); assert.equal(connected.mediaCalls(), 1);
});

test('mute controls microphone tracks without suppressing remote speech and duration limit closes capture', async () => {
  const env = await connect(setup({ maxDurationMs: 1000 }));
  assert.equal(env.voice.setMuted(true), true); assert.equal(env.track.enabled, false); assert.equal(env.voice.snapshot.muted, true);
  env.peer.channel.event(created('resp_1')); env.peer.channel.event({ type: 'output_audio_buffer.started', response_id: 'resp_1' });
  assert.equal(env.voice.snapshot.assistantSpeaking, true);
  env.voice.setMuted(false); assert.equal(env.track.enabled, true);
  await env.clock.advance(1000); assert.equal(env.voice.snapshot.phase, 'idle'); assert.equal(env.track.stopped, 1);
  assert.deepEqual(env.releases, ['rtc_test']);
});

test('autoplay denial and microphone rejection become meaningful errors and release capture', async () => {
  const denied = Object.assign(new Error('permission'), { name: 'NotAllowedError' });
  const missing = setup({ getUserMedia: async () => { throw denied; } });
  await assert.rejects(missing.voice.start(), /权限/); assert.equal(missing.voice.snapshot.active, false); assert.equal(missing.peerCalls(), 0);
  const audio = new Audio(); audio.play = () => Promise.reject(denied);
  const env = await connect(setup({ audioFactory: () => audio }));
  env.peer.emit('track', { streams: [env.remote] }); await turns();
  assert.equal(env.voice.snapshot.phase, 'error'); assert.match(env.voice.snapshot.error, /权限/);
  assert.equal(env.track.stopped, 1); assert.equal(env.remoteTrack.stopped, 1); assert.equal(env.peer.closed, true);
});

test('context is bounded, deduplicated and cannot itself initiate a model response', async () => {
  const env = await connect(setup());
  const before = env.peer.channel.sent.length;
  assert.equal(env.voice.updateContext('当前空间：专注'), false); assert.equal(env.peer.channel.sent.length, before);
  assert.equal(env.voice.updateContext('界'.repeat(9000)), true);
  const context = env.peer.channel.sent.at(-1);
  assert.equal(context.item.role, 'system'); assert.ok(context.item.content[0].text.length < 8100);
  assert.equal(env.peer.channel.sent.some(e => e.type === 'response.create'), false);
  await env.voice.stop(); assert.equal(env.voice.updateContext('closed'), false);
});

test('partial arguments never dispatch, malformed completed arguments return an error without running a tool', async () => {
  const env = await connect(setup());
  env.peer.channel.event(created('resp_1'));
  env.peer.channel.event({ ...tool(), type: 'response.function_call_arguments.delta', delta: '{"text":' });
  await turns(); assert.equal(env.tools.length, 0);
  env.peer.channel.event(tool('bad', 'resp_1', '{"text":'));
  env.peer.channel.event(done('resp_1')); await turns();
  assert.equal(env.tools.length, 0);
  const output = env.peer.channel.sent.find(event => event.item?.type === 'function_call_output');
  assert.equal(JSON.parse(output.item.output).error, 'invalid_tool_arguments');
  await env.voice.stop();
});

test('a completed response alone can deliver a tool call, and failed tools are not replayed', async () => {
  let count = 0;
  const env = await connect(setup({ onToolCall: async () => { count++; throw Object.assign(new Error('lost acknowledgement'), { outcome_unknown: true }); } }));
  env.peer.channel.event(created('resp_1'));
  const output = [{ type: 'function_call', name: 'agentos_intent', call_id: 'once', arguments: '{"text":"记一下 茶"}' }];
  env.peer.channel.event(done('resp_1', output)); await turns();
  env.peer.channel.event(done('resp_1', output)); await turns();
  assert.equal(count, 1);
  const result = env.peer.channel.sent.find(event => event.item?.type === 'function_call_output');
  assert.equal(JSON.parse(result.item.output).outcome_unknown, true);
  await env.voice.stop();
});

test('resource limits stop rather than forgetting old call identifiers and replaying old actions', async () => {
  const env = await connect(setup());
  env.peer.channel.event(created('resp_1'));
  for (let i = 0; i <= 100; i++) env.peer.channel.event(tool(`call_${i}`));
  await turns();
  assert.equal(env.voice.snapshot.phase, 'error'); assert.equal(env.track.stopped, 1); assert.equal(env.peer.closed, true);
  assert.equal(env.tools.length, 0);
  env.peer.channel.event(tool('call_0')); await turns(); assert.equal(env.tools.length, 0);
});

test('an independently aborted SDP request fails and cleans capture rather than pretending the user stopped', async () => {
  const env = setup({ createSession: async () => { throw Object.assign(new Error('HTTP timeout'), { name: 'AbortError' }); } });
  await assert.rejects(env.voice.start(), /连接未完成/);
  assert.equal(env.voice.snapshot.phase, 'error'); assert.equal(env.voice.snapshot.active, false);
  assert.equal(env.track.stopped, 1); assert.equal(env.peer.closed, true); assert.equal(env.clock.timers.size, 0);
});

test('tool result byte budget accepts a 16100-byte response and preserves oversized mutation receipts', async () => {
  for (const result of [
    { ok: true, accepted: true, message: 'a'.repeat(16100), task: { id: 8, state: 'queued' } },
    { ok: false, accepted: true, outcome_unknown: true, message: '界'.repeat(10000), task: { id: 9, state: 'running' } },
  ]) {
    const env = await connect(setup({ onToolCall: async () => result }));
    env.peer.channel.event(created('resp_1')); env.peer.channel.event(tool()); env.peer.channel.event(done('resp_1')); await turns();
    const event = env.peer.channel.sent.find(value => value.item?.type === 'function_call_output');
    const received = JSON.parse(event.item.output);
    assert.equal(received.accepted, true); assert.equal(received.ok, result.ok);
    assert.deepEqual(received.task, result.task);
    assert.ok(new TextEncoder().encode(event.item.output).byteLength <= 16384);
    if (result.outcome_unknown) { assert.equal(received.outcome_unknown, true); assert.equal(received.truncated, true); }
    else assert.equal(received.message.length, 16100);
    await env.voice.stop();
  }
});

test('ongoing context redacts pasted credential patterns before sending it to the voice model', async () => {
  const env = await connect(setup());
  env.voice.updateContext('笔记：sk-proj-test_only_not_a_real_key；Authorization: Bearer sample.credential-123456');
  const text = env.peer.channel.sent.at(-1).item.content[0].text;
  assert.ok(text.includes('[redacted]')); assert.ok(text.includes('Bearer [redacted]'));
  assert.equal(text.includes('test_only_not_a_real_key'), false); assert.equal(text.includes('sample.credential-123456'), false);
  await env.voice.stop();
});
