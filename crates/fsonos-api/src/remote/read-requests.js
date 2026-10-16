// Coalesce a burst of JSON observations before dispatch, never cache replies.
'use strict';

const ReadRequests = (() => {
  const MAX_PENDING = 64;
  const options = new Set(['method', 'headers', 'cache', 'signal']);
  const abortReason = (signal) => signal.reason ?? new DOMException('Request cancelled', 'AbortError');
  const copy = (value) => typeof structuredClone === 'function'
    ? structuredClone(value) : JSON.parse(JSON.stringify(value));

  function keyOf(where, init) {
    // Only the remote's ordinary JSON reads are interchangeable. Additional
    // request options or headers must not accidentally share another caller's
    // observation. Event streams/long polls have their own delivery lifecycle.
    if (init.method !== 'GET' || typeof where !== 'string' || !where.startsWith('/')
        || where.startsWith('//') || where.split('?')[0] === '/events'
        || Object.keys(init).some((key) => !options.has(key))
        || init.cache !== 'no-store' || !init.headers) return null;
    const headers = Object.entries(init.headers);
    if (headers.length !== 1 || headers[0][0].toLowerCase() !== 'accept'
        || headers[0][1] !== 'application/json') return null;
    // Do not normalize names, query values, or pagination revisions: similar
    // paths may select different households or different queue observations.
    return where;
  }

  function create(send, { limit = MAX_PENDING } = {}) {
    if (typeof send !== 'function' || !Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PENDING) {
      throw new TypeError('A request function and a bounded pending-read limit are required.');
    }
    const pending = new Map();
    const active = new Set();
    let commands = 0;
    const forget = (key, entry) => { if (pending.get(key) === entry) pending.delete(key); };

    function observe(entry, signal) {
      entry.waiters++;
      return new Promise((resolve, reject) => {
        let done = false;
        const finish = (ok, value) => {
          if (done) return;
          done = true;
          signal?.removeEventListener('abort', cancelled);
          entry.waiters--;
          if (!entry.settled && entry.waiters === 0) {
            forget(entry.key, entry);
            entry.controller.abort();
          }
          if (ok) resolve(value); else reject(value);
        };
        const cancelled = () => finish(false, abortReason(signal));
        signal?.addEventListener('abort', cancelled, { once: true });
        if (signal?.aborted) cancelled();
        entry.promise.then((value) => {
          if (done) return;
          // Callers previously got separate JSON parses. Preserve that property:
          // one panel must not mutate a sibling panel's copy of a shared reply.
          try { finish(true, copy(value)); } catch (error) { finish(false, error); }
        }, (error) => finish(false, error));
      });
    }

    const request = async function request(where, init) {
      if (init.signal?.aborted) throw abortReason(init.signal);
      if (init.method !== 'GET') {
        // Neither pre-command reads nor observations made while a command runs
        // may satisfy a later preflight. Failed/uncertain commands are barriers
        // too. Existing readers still receive their own in-flight observations.
        commands++;
        pending.clear();
        try { return await send(where, init); }
        finally { commands--; pending.clear(); }
      }
      const key = commands ? null : keyOf(where, init);
      if (key === null) return send(where, init);
      let entry = pending.get(key);
      if (!entry) {
        // Overflow bypasses sharing, not the transport's own deadlines/limits.
        if (pending.size >= limit) return send(where, init);
        entry = { key, controller: new AbortController(), waiters: 0, settled: false, promise: null };
        pending.set(key, entry);
        active.add(entry);
        entry.promise = Promise.resolve().then(() => {
          // Close admission BEFORE sending. A preflight or event-driven read
          // arriving later must start a new exchange, not join an older one.
          forget(key, entry);
          if (entry.controller.signal.aborted) throw abortReason(entry.controller.signal);
          return send(where, { ...init, signal: entry.controller.signal });
        }).then((value) => {
          entry.settled = true; active.delete(entry); forget(key, entry);
          if (entry.controller.signal.aborted) throw abortReason(entry.controller.signal);
          return value;
        }, (error) => {
          entry.settled = true; active.delete(entry); forget(key, entry); throw error;
        });
      }
      return observe(entry, init.signal);
    };
    request.cancelReads = () => {
      pending.clear();
      for (const entry of active) entry.controller.abort();
    };
    return request;
  }
  return { create, keyOf, MAX_PENDING };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = ReadRequests;
