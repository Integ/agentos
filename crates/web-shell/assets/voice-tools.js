/**
 * Realtime may propose only these application tools. Speech transcripts are
 * captions, never executable commands. Runtime authorization still belongs to
 * AgentOS, and this router deliberately exposes no approval endpoint.
 */

const SPACES = new Set(['overview', 'focus', 'memory', 'activity', 'clock', 'help']);
const ACTIONS = new Set(['start', 'pause', 'reset']);
const DURATIONS = new Set([25, 45, 60, null]);
const MAX_ARGUMENT_BYTES = 16_384;
const MAX_RESULT_BYTES = 16_384;
const MAX_CALLS = 100;
const encoder = new TextEncoder();

const plainObject = value => value !== null && typeof value === 'object' && !Array.isArray(value)
  && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
const exactKeys = (value, names) => {
  const keys = Object.keys(value);
  return keys.length === names.length && keys.every(key => names.includes(key))
    && keys.every(key => Object.hasOwn(Object.getOwnPropertyDescriptor(value, key), 'value'));
};
const bytes = value => encoder.encode(value).byteLength;
const resultCopy = value => JSON.parse(JSON.stringify(value));
const invalid = () => ({ ok: false, error: 'invalid_tool', message: '这个语音工具或参数不可用。' });
const cancelled = () => ({ ok: false, cancelled: true, error: 'cancelled_before_dispatch' });

/** Return a detached allowlisted command; unknown fields fail as a whole. */
export function validateVoiceTool(name, args) {
  if (!plainObject(args)) return null;
  if (name === 'agentos_intent' && exactKeys(args, ['text'])
    && typeof args.text === 'string' && args.text.trim() && bytes(args.text) <= MAX_ARGUMENT_BYTES) {
    return { name, args: { text: args.text.trim() } };
  }
  if (name === 'show_space' && exactKeys(args, ['space']) && SPACES.has(args.space)) {
    return { name, args: { space: args.space } };
  }
  if (name === 'focus_timer' && exactKeys(args, ['action', 'minutes'])
    && ACTIONS.has(args.action) && DURATIONS.has(args.minutes)) {
    return { name, args: { action: args.action, minutes: args.minutes } };
  }
  return null;
}

function sensitiveKey(key) {
  const normalized = key.toLowerCase().replace(/[^a-z0-9]/g, '');
  return normalized === 'confirmation' || normalized === 'proto' || normalized === 'prototype'
    || normalized === 'constructor'
    || ['token', 'apikey', 'password', 'secret', 'authorization', 'credential', 'cookie']
      .some(part => normalized.includes(part));
}

function safeText(value, length = 1500) {
  return value.slice(0, length)
    .replace(/\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{8,}\b/g, '[redacted]')
    .replace(/\bBearer\s+[A-Za-z0-9._~+/-]+=*/gi, 'Bearer [redacted]');
}

/** Bound both traversal work and the serialized result, even for hostile data. */
function sanitizeResult(value) {
  const seen = new WeakSet();
  let nodes = 0;
  const visit = (item, depth = 0) => {
    if (++nodes > 256 || depth > 5) return '[bounded preview]';
    if (item === null || typeof item === 'boolean') return item;
    if (typeof item === 'number') return Number.isFinite(item) ? item : null;
    if (typeof item === 'string') return safeText(item);
    if (typeof item !== 'object') return null;
    if (seen.has(item)) return '[circular]';
    seen.add(item);
    if (Array.isArray(item)) return item.slice(0, 30).map(child => visit(child, depth + 1));
    if (!plainObject(item)) return '[unsupported value]';
    const result = Object.create(null);
    for (const key of Object.keys(item).slice(0, 30)) {
      if (sensitiveKey(key)) continue;
      const descriptor = Object.getOwnPropertyDescriptor(item, key);
      if (descriptor && Object.hasOwn(descriptor, 'value')) result[safeText(key, 80)] = visit(descriptor.value, depth + 1);
    }
    return result;
  };
  const sanitized = visit(value);
  const result = plainObject(sanitized)
    ? { ...sanitized, ok: typeof sanitized.ok === 'boolean' ? sanitized.ok : !sanitized.error }
    : { ok: true, result: sanitized };
  if (bytes(JSON.stringify(result)) <= MAX_RESULT_BYTES) return result;
  // Do not truncate JSON or silently lose whether an action took effect.
  const bounded = { ok: result.ok, truncated: true,
    message: '工具结果超过语音上下文限额；请查看任务详情。' };
  for (const key of ['outcome_unknown', 'cancelled', 'accepted', 'requires_confirmation']) if (typeof result[key] === 'boolean') bounded[key] = result[key];
  for (const key of ['status', 'error', 'task_id', 'scene', 'mode']) {
    if (typeof result[key] === 'string') bounded[key] = safeText(result[key], 80);
    else if (typeof result[key] === 'number' && Number.isFinite(result[key])) bounded[key] = result[key];
  }
  if (plainObject(result.task)) {
    bounded.task = {};
    if (typeof result.task.id === 'number' && Number.isFinite(result.task.id)) bounded.task.id = result.task.id;
    else if (typeof result.task.id === 'string') bounded.task.id = safeText(result.task.id, 80);
    if (typeof result.task.state === 'string') bounded.task.state = safeText(result.task.state, 80);
  }
  return bounded;
}

function executionError(error, toolName) {
  const unknown = error?.outcome_unknown === true || error?.unknown === true || error?.ambiguous === true
    || (toolName === 'agentos_intent' && error?.name === 'AbortError');
  return { ok: false, error: unknown ? 'outcome_unknown' : 'execution_error', outcome_unknown: unknown,
    message: unknown ? '操作可能已经生效。先查看任务状态，不要直接重发。' : '这次操作没有得到成功确认，请查看界面中的结果。' };
}

/**
 * Serialize application actions and keep a per-session exactly-once ledger.
 * Aborting before dispatch cancels a queued call. Once a callback starts, its
 * actual result settles normally: interrupting speech cannot undo an OS task.
 *
 * reset() invalidates queued calls and clears the old session ledger. It does
 * not cancel callbacks already dispatched, and new work waits for those calls.
 * At 100 unique calls, refuse more until a new session; evicting old IDs could
 * accidentally replay an old mutation. Every returned value is a detached copy.
 */
export class VoiceToolRouter {
  #callbacks;
  #calls = new Map();
  #tail = Promise.resolve();
  #generation = 0;

  constructor({ performIntent, showSpace, controlFocus } = {}) {
    if ([performIntent, showSpace, controlFocus].some(callback => typeof callback !== 'function')) {
      throw new TypeError('VoiceToolRouter requires all application callbacks');
    }
    this.#callbacks = { performIntent, showSpace, controlFocus };
  }

  execute(call, { signal } = {}) {
    if (!plainObject(call) || typeof call.callId !== 'string' || !/^[A-Za-z0-9_.-]{1,200}$/.test(call.callId)) {
      return Promise.resolve(invalid());
    }
    let args = call.arguments;
    if (typeof args === 'string') {
      if (bytes(args) > MAX_ARGUMENT_BYTES) return Promise.resolve(invalid());
      try { args = JSON.parse(args); } catch { return Promise.resolve(invalid()); }
    }
    const validated = validateVoiceTool(call.name, args);
    if (!validated) return Promise.resolve(invalid());
    const signature = JSON.stringify(validated);
    const existing = this.#calls.get(call.callId);
    if (existing) return existing.signature === signature
      ? existing.promise.then(resultCopy)
      : Promise.resolve({ ok: false, error: 'conflicting_call_id', message: '重复调用的参数不一致，未再次执行。' });
    if (this.#calls.size >= MAX_CALLS) {
      return Promise.resolve({ ok: false, error: 'session_tool_limit', message: '本次语音会话已到操作上限，请重新连接后继续。' });
    }
    let resolve;
    const promise = new Promise(done => { resolve = done; });
    const record = { signature, promise, resolve, validated, generation: this.#generation,
      signal, started: false, settled: false, abort: null };
    this.#calls.set(call.callId, record);
    record.abort = () => { if (!record.started) this.#settle(record, cancelled()); };
    if (signal?.aborted) record.abort();
    else signal?.addEventListener('abort', record.abort, { once: true });
    this.#tail = this.#tail.then(() => this.#dispatch(record)).catch(() => {
      this.#settle(record, executionError(null, validated.name));
    });
    return promise.then(resultCopy);
  }

  reset() {
    this.#generation++;
    for (const record of this.#calls.values()) if (!record.started) this.#settle(record, cancelled());
    this.#calls.clear();
  }

  #settle(record, result) {
    if (record.settled) return;
    record.settled = true;
    record.signal?.removeEventListener('abort', record.abort);
    record.resolve(sanitizeResult(result));
  }

  async #dispatch(record) {
    if (record.settled) return;
    if (record.signal?.aborted || record.generation !== this.#generation) {
      this.#settle(record, cancelled()); return;
    }
    record.started = true;
    const { name, args } = record.validated;
    try {
      const result = name === 'agentos_intent' ? await this.#callbacks.performIntent(args.text)
        : name === 'show_space' ? await this.#callbacks.showSpace(args.space)
          : await this.#callbacks.controlFocus(args);
      this.#settle(record, result);
    } catch (error) { this.#settle(record, executionError(error, name)); }
  }
}
