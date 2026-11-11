/**
 * Coordinates layout generation only. It never submits or retries OS tasks.
 *
 * `request(context, { signal })` must return an already validated JSON response
 * containing `scene`. `onScene(response, key)` receives a detached copy. Status
 * callbacks receive { state, key, error? }; error is an Error on failure.
 *
 * update/retry return whether a new selection was accepted. An unchanged key
 * stays settled, even after a failure or TTL expiry, until retry is explicit.
 * reset clears the current selection and cache, emitting idle. dispose clears
 * everything without callbacks. Both invalidate even non-cooperative requests.
 */
export class LayoutCoordinator {
  #request;
  #onScene;
  #onStatus;
  #debounceMs;
  #cacheSize;
  #cacheTtlMs;
  #now;
  #setTimer;
  #clearTimer;
  #cache = new Map();
  #desired = null;
  #timer = null;
  #controller = null;
  #version = 0;
  #disposed = false;

  constructor({ request, onScene = () => {}, onStatus = () => {},
    debounceMs = 450, cacheSize = 8, cacheTtlMs = 120000,
    now = () => Date.now(), setTimer = (callback, delay) => setTimeout(callback, delay),
    clearTimer = timer => clearTimeout(timer) } = {}) {
    if (typeof request !== 'function') throw new TypeError('request must be a function');
    if (typeof onScene !== 'function' || typeof onStatus !== 'function') throw new TypeError('callbacks must be functions');
    if (!Number.isFinite(debounceMs) || debounceMs < 0
      || !Number.isInteger(cacheSize) || cacheSize < 0 || cacheSize > 64
      || !Number.isFinite(cacheTtlMs) || cacheTtlMs < 0) throw new TypeError('invalid coordinator limits');
    this.#request = request;
    this.#onScene = onScene;
    this.#onStatus = onStatus;
    this.#debounceMs = debounceMs;
    this.#cacheSize = cacheSize;
    this.#cacheTtlMs = cacheTtlMs;
    this.#now = now;
    this.#setTimer = setTimer;
    this.#clearTimer = clearTimer;
  }

  update(key, context) {
    if (this.#disposed) return false;
    if (typeof key !== 'string' || !key) throw new TypeError('key must be a nonempty string');
    if (this.#desired?.key === key) return false;
    return this.#select(key, context, false);
  }

  retry() {
    if (this.#disposed || !this.#desired) return false;
    return this.#select(this.#desired.key, this.#desired.context, true);
  }

  reset() {
    if (this.#disposed) return false;
    this.#version++;
    this.#cancel();
    this.#desired = null;
    this.#cache.clear();
    this.#onStatus({ state: 'idle', key: null });
    return true;
  }

  dispose() {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#version++;
    this.#cancel();
    this.#desired = null;
    this.#cache.clear();
  }

  #cancel() {
    if (this.#timer !== null) this.#clearTimer(this.#timer);
    this.#timer = null;
    this.#controller?.abort();
    this.#controller = null;
  }

  #current(version, key) {
    return !this.#disposed && version === this.#version && this.#desired?.key === key;
  }

  #select(key, context, force) {
    const version = ++this.#version;
    this.#cancel();
    // Record the selection before invoking hooks: onScene may synchronously
    // render the UI and call update again with exactly this same key.
    this.#desired = { key, context };
    if (force) this.#cache.delete(key);
    const cached = this.#cached(key);
    if (cached) {
      this.#deliver(cached, version, key);
      return true;
    }
    this.#onStatus({ state: 'generating', key });
    if (!this.#current(version, key)) return true;
    this.#timer = this.#setTimer(() => {
      this.#timer = null;
      void this.#run(version, key, context);
    }, force ? 0 : this.#debounceMs);
    return true;
  }

  #cached(key) {
    const entry = this.#cache.get(key);
    if (!entry) return null;
    if (entry.expiresAt <= this.#now()) {
      this.#cache.delete(key);
      return null;
    }
    this.#cache.delete(key);
    this.#cache.set(key, entry);
    return entry.response;
  }

  #remember(key, response) {
    if (!this.#cacheSize || !this.#cacheTtlMs) return;
    const now = this.#now();
    for (const [cachedKey, entry] of this.#cache) if (entry.expiresAt <= now) this.#cache.delete(cachedKey);
    this.#cache.delete(key);
    this.#cache.set(key, { response, expiresAt: now + this.#cacheTtlMs });
    while (this.#cache.size > this.#cacheSize) this.#cache.delete(this.#cache.keys().next().value);
  }

  #deliver(response, version, key) {
    if (!this.#current(version, key)) return;
    try {
      this.#onScene(cloneResponse(response), key);
      if (this.#current(version, key)) this.#onStatus({ state: 'ready', key });
    } catch (error) {
      this.#cache.delete(key);
      if (this.#current(version, key)) this.#onStatus({ state: 'fallback', key, error: asError(error) });
    }
  }

  async #run(version, key, context) {
    if (!this.#current(version, key)) return;
    const controller = new AbortController();
    this.#controller = controller;
    try {
      const response = cloneResponse(await this.#request(context, { signal: controller.signal }));
      if (!this.#current(version, key) || controller.signal.aborted) return;
      if (this.#controller === controller) this.#controller = null;
      this.#remember(key, response);
      this.#deliver(response, version, key);
    } catch (error) {
      if (!this.#current(version, key) || controller.signal.aborted) return;
      if (this.#controller === controller) this.#controller = null;
      this.#onStatus({ state: 'fallback', key, error: asError(error) });
    }
  }
}

function asError(error) {
  return error instanceof Error ? error : new Error(String(error));
}

function cloneResponse(response) {
  if (!response || typeof response !== 'object' || !response.scene || typeof response.scene !== 'object') {
    throw new TypeError('layout request returned no scene');
  }
  // Responses are bounded, validated JSON at the caller's protocol boundary.
  // Detached copies prevent renderer mutation from contaminating cached plans.
  return JSON.parse(JSON.stringify(response));
}
