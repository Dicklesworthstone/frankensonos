// Read-only, heartbeat-aware delivery for the daemon's existing event stream.
// Native EventSource hides comments, so it cannot expose a silent body stall.
'use strict';
const RemoteEventFeed = (() => {
  const CONNECT_TIMEOUT = 15000, IDLE_TIMEOUT = 45000;
  const FIRST_RETRY = 1000, MAX_RETRY = 60000, MAX_FRAME = 1024 * 1024;
  const cooldown = { until: 0 };
  const monotonic = () => globalThis.performance?.now() ?? Date.now();
  const failure = (message, protocol = false) => Object.assign(new Error(message), { protocol });

  // Incremental SSE parsing: CR, LF, CRLF and a leading BOM may cross chunks.
  // Only a complete block commits its cursor. EOF never dispatches a fragment.
  function parser({ event, cursor = () => {}, retry = () => {}, activity = () => {},
    lastId = '', limit = MAX_FRAME }) {
    let line = '', data = [], kind = '', id = lastId, size = 0, first = true, cr = false, live = true;
    function consume() {
      if (!line) {
        cursor(id);
        const value = data.length ? { type: kind || 'message', data: data.join('\n'), lastEventId: id } : null;
        data = []; kind = ''; size = 0;
        activity();
        if (value) event(value);
      } else if (line[0] !== ':') {
        const split = line.indexOf(':');
        const field = split < 0 ? line : line.slice(0, split);
        let value = split < 0 ? '' : line.slice(split + 1);
        if (value[0] === ' ') value = value.slice(1);
        if (field === 'data') data.push(value);
        else if (field === 'event') kind = value;
        else if (field === 'id' && !value.includes('\0')) id = value;
        else if (field === 'retry' && /^[0-9]+$/.test(value)) retry(Number(value));
      }
      line = '';
    }
    return {
      push(text) {
        for (const char of text) {
          if (!live) return;
          if (first) { first = false; if (char === '\ufeff') continue; }
          if (cr) { cr = false; if (char === '\n') continue; }
          size += char.length;
          if (size > limit) throw failure('The event frame exceeds the receive limit.', true);
          if (char === '\r' || char === '\n') { consume(); cr = char === '\r'; }
          else line += char;
        }
      },
      clearId() { id = ''; },
      stop() { live = false; line = ''; data = []; },
    };
  }

  function retryAfter(value, now) {
    if (typeof value !== 'string' || !value.trim()) return 0;
    const text = value.trim();
    if (/^[0-9]+$/.test(text)) return Number(text) * 1000;
    if (/^[+-]?[0-9.]+$/.test(text)) return 0;
    const date = Date.parse(text);
    return Number.isFinite(date) ? Math.max(0, date - now) : 0;
  }

  // The daemon's cursors are decimal u64s. Keep them as strings: converting
  // them to JavaScript numbers loses identity above the safe-integer range.
  function validId(id) {
    if (id === '') return true;
    return /^[0-9]{1,20}$/.test(id)
      && (id.length < 20 || id <= '18446744073709551615');
  }

  function open(url = '/events', { fetcher = (...args) => fetch(...args),
    schedule = setTimeout, cancel = clearTimeout, now = monotonic, wallNow = Date.now,
    random = Math.random, gate = cooldown } = {}) {
    if (url !== '/events') throw new Error('Live events must use the daemon event endpoint.');
    let closed = false, readyState = 0, lastId = '', backoff = FIRST_RETRY, serverRetry = 0;
    let abortAttempt = null, wakeRetry = null, retryTimer = null;
    const handlers = new Map();
    const emit = (type, detail = {}) => {
      for (const listener of [...(handlers.get(type) || [])]) {
        if (closed) break;
        try { listener({ ...detail, type }); } catch (_) { /* A view cannot restart delivery. */ }
      }
    };
    const discard = (body) => {
      try { void body?.cancel().catch(() => {}); } catch (_) { /* Cleanup cannot delay recovery. */ }
    };
    const wait = (until) => new Promise((resolve) => {
      const done = () => {
        if (retryTimer !== null) cancel(retryTimer);
        retryTimer = null; wakeRetry = null; resolve();
      };
      wakeRetry = done;
      const step = () => {
        retryTimer = null;
        if (closed || now() >= until) { done(); return; }
        // A server delay is a minimum, never shortened to our backoff cap.
        // Infinite/unrepresentable hints wait for close without a timer storm.
        if (Number.isFinite(until)) retryTimer = schedule(step, Math.min(MAX_RETRY, until - now()));
      };
      step();
    });

    async function receive() {
      const controller = new AbortController();
      let done = false, deadline = null, reader = null, decoding = null;
      let rejectStopped;
      const stopped = new Promise((_, reject) => { rejectStopped = reject; });
      const cleanup = () => {
        if (done) return;
        done = true;
        if (deadline !== null) cancel(deadline);
        deadline = null; decoding?.stop();
        controller.abort();
        discard(reader);
      };
      const stop = (error) => { if (!done) { cleanup(); rejectStopped(error); } };
      const arm = (ms) => {
        if (deadline !== null) cancel(deadline);
        deadline = schedule(() => stop(failure('The live connection stopped responding.')), ms);
      };
      abortAttempt = stop;
      arm(CONNECT_TIMEOUT);
      const work = async () => {
        const response = await fetcher(url, { method: 'GET', headers: {
          Accept: 'text/event-stream', ...(lastId ? { 'Last-Event-ID': lastId } : {}),
        }, credentials: 'same-origin', redirect: 'error', cache: 'no-store', signal: controller.signal });
        if (done || closed) { discard(response.body); return; }
        if (response.status !== 200) {
          const error = failure(`Live events unavailable (${response.status}).`);
          error.status = response.status;
          error.retryAfter = retryAfter(response.headers?.get('retry-after'), wallNow());
          discard(response.body); throw error;
        }
        if (response.headers?.get('content-type')?.split(';')[0].trim().toLowerCase() !== 'text/event-stream'
            || typeof response.body?.getReader !== 'function') {
          discard(response.body); throw failure('The live response is not an event stream.', true);
        }
        reader = response.body.getReader();
        const decoder = new TextDecoder();
        decoding = parser({ lastId,
          cursor(id) { if (!validId(id)) throw failure('The event cursor is unreadable.', true); lastId = id; },
          retry(ms) { serverRetry = ms; },
          activity() { if (!done && !closed) { backoff = FIRST_RETRY; arm(IDLE_TIMEOUT); } },
          event(value) {
            if (done || closed) return;
            // A restarted journal may have no numbered events yet. Do not
            // carry the previous daemon's cursor back on the next connection.
            if (value.type === 'events.reset') { lastId = ''; decoding.clearId(); }
            emit(value.type, value);
          },
        });
        arm(IDLE_TIMEOUT);
        readyState = 1; emit('open');
        while (!done && !closed) {
          const chunk = await reader.read();
          if (done || closed) return;
          if (chunk.done) throw failure('The live stream ended.');
          decoding.push(decoder.decode(chunk.value, { stream: true }));
        }
      };
      try { await Promise.race([work(), stopped]); }
      finally { cleanup(); if (abortAttempt === stop) abortAttempt = null; }
    }

    async function run() {
      while (!closed) {
        await wait(gate.until);
        if (closed) return;
        let error;
        try { await receive(); }
        catch (caught) { error = caught; }
        if (closed) return;
        if (error?.protocol) lastId = '';
        readyState = 0;
        const delay = Math.max(backoff + Math.floor(random() * backoff / 4), serverRetry, error?.retryAfter || 0);
        gate.until = Math.max(gate.until, now() + delay);
        backoff = Math.min(MAX_RETRY, backoff * 2);
        emit('error', { error });
      }
    }

    const feed = {
      get readyState() { return readyState; },
      addEventListener(type, listener) {
        if (typeof listener !== 'function') return;
        if (!handlers.has(type)) handlers.set(type, new Set());
        handlers.get(type).add(listener);
      },
      removeEventListener(type, listener) { handlers.get(type)?.delete(listener); },
      close() {
        if (closed) return;
        closed = true; readyState = 2;
        abortAttempt?.(failure('Live updates stopped.'));
        wakeRetry?.(); handlers.clear();
      },
    };
    // Registration happens before any injected fetch can publish an event.
    void Promise.resolve().then(run).catch(() => { feed.close(); });
    return feed;
  }

  return { open, parser, retryAfter, validId, CONNECT_TIMEOUT, IDLE_TIMEOUT, FIRST_RETRY, MAX_RETRY, MAX_FRAME };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = RemoteEventFeed;
