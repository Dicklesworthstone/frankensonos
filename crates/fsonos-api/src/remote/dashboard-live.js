// One event-driven snapshot lane for the Listening Room; never sends commands.
'use strict';
const DashboardLive = (() => {
  const COALESCE = 350, FALLBACK = 15000, MAX_BACKOFF = 60000;
  const EVENTS = ['zone.state', 'topology.changed', 'player.health', 'action.logged', 'dj.status'];
  function create({ connect = null, reload, invalidate = () => {}, connection = () => {},
    blocked = () => false, retryAfter = () => 0, observed = () => {}, demo = false,
    eventTargets = () => null, failedTargets = () => null, reconciled = ok => ok,
    schedule = setTimeout, cancel = clearTimeout, now = () => globalThis.performance?.now() ?? Date.now() }) {
    let active = false, disposed = false, busy = false, connected = false;
    let pending = false, full = false, explicit = false, failed = false, generation = 0, lifetime = 0;
    const targets = new Set();
    let feed = null, timer = null, timerAt = 0, wait = FALLBACK, notBefore = 0, retryAt = 0;
    const clear = () => { if (timer !== null) cancel(timer); timer = null; };
    const notify = (fn, ...args) => { try { fn(...args); } catch (_) { /* Views do not own delivery. */ } };
    const addresses = value => Array.isArray(value) && value.length <= 256
      && value.every(v => typeof v === 'string' && v.trim() && v.length <= 4096) ? [...new Set(value)] : null;
    const failures = () => { try { return addresses(failedTargets()); } catch (_) { return null; } };
    function eventScope(name, event) {
      if (!['zone.state', 'player.health', 'dj.status'].includes(name)) return null;
      try {
        // Read only the event's identity. Playback still comes from the public
        // snapshot API; room names can identify groups in several households.
        if (typeof event?.data !== 'string' || event.data.length > 1048576) return null;
        const value = JSON.parse(event.data);
        if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
        const found = addresses(eventTargets(name === 'dj.status' ? value.zone : value.room));
        return found?.length ? found : null;
      } catch (_) { return null; }
    }
    function recoveryFloor() {
      if (!failed) return 0;
      const unavailable = failures();
      // An offline room's retry delay must not hold healthy room updates. A
      // daemon-wide Retry-After, unlike this recovery delay, still gates both.
      return pending && !full && !explicit && unavailable
        && [...targets].some(target => !unavailable.includes(target)) ? 0 : retryAt;
    }
    function arm(delay = COALESCE) {
      if (!active || busy || disposed) return;
      const due = Math.max(now() + delay, notBefore, recoveryFloor());
      if (!Number.isFinite(due)) { clear(); return; }
      const at = Math.min(due, now() + MAX_BACKOFF);
      if (timer !== null && timerAt <= at) return;
      clear(); timerAt = at;
      timer = schedule(() => { timer = null; void run(); }, Math.max(0, at - now()));
    }
    function request(user = false, hard = false, affected = null) {
      if (!active || disposed) return;
      ++generation; pending = true; explicit ||= user;
      if (user || affected === null) full = true;
      else for (const target of affected) targets.add(target);
      if (targets.size > 256) full = true;
      if (full) targets.clear();
      notify(invalidate, hard, full ? null : affected);
      arm(user ? 0 : COALESCE);
    }
    function open() {
      if (feed || !active || !connect || demo) return;
      let next;
      try { next = connect(); } catch (_) { return; }
      if (!next) return;
      feed = next;
      const current = () => active && feed === next;
      next.addEventListener('open', () => {
        if (!current()) return;
        connected = true;
        notify(connection, false);
        request(false, true);
      });
      next.addEventListener('error', () => {
        if (!current()) return;
        connected = false;
        // A browser CLOSED feed cannot reconnect itself; CONNECTING feeds,
        // including RemoteEventFeed, retain their own cursor and retry pace.
        if (next.readyState === 2) { next.close(); feed = null; }
        notify(connection, false);
        request(false, true);
      });
      for (const name of EVENTS) next.addEventListener(name, event => {
        if (current()) request(false, false, eventScope(name, event));
      });
      for (const name of ['events.reset', 'reset']) next.addEventListener(name, () => {
        if (current()) { notify(connection, false); request(false, true); }
      });
    }
    async function run() {
      if (!active || disposed || busy) return;
      if (now() < Math.max(notBefore, recoveryFloor())) { arm(0); return; }
      if (blocked()) { arm(1000); return; }
      open();
      const version = generation, life = lifetime, user = explicit;
      const unavailable = failures();
      let scope = null;
      if (!full && !user) {
        if (failed && connected && unavailable && now() >= retryAt) scope = { recovery: true };
        else if (targets.size) {
          const ready = [...targets].filter(target => !failed || now() >= retryAt
            || (unavailable && !unavailable.includes(target)));
          if (!ready.length) { arm(0); return; }
          scope = { targets: ready };
        }
      }
      if (scope?.targets) {
        for (const target of scope.targets) targets.delete(target);
        pending = targets.size > 0;
      } else { targets.clear(); pending = false; }
      explicit = full = false; busy = true;
      let ok = false, minimum = 0;
      try { ok = await reload(user, scope) === true; minimum = retryAfter(); }
      catch (error) { minimum = error?.retryAfter || 0; }
      finally {
        // A server minimum remains binding across hide/show and reconnects.
        if (typeof minimum === 'number' && minimum > 0) notBefore = Math.max(notBefore, now() + minimum);
        busy = false;
        if (active && !disposed) {
          if (life === lifetime) {
            const current = version === generation;
            let usable = false;
            try { usable = reconciled(ok) === true; } catch (_) { /* Remain stale. */ }
            if (current || usable) {
              const remaining = failures();
              const needsRecovery = remaining ? remaining.length > 0 : !ok;
              if (needsRecovery) {
                const attempted = !failed || !ok || !scope || scope.recovery
                  || scope.targets.some(target => unavailable?.includes(target));
                if (attempted) { wait = Math.min(MAX_BACKOFF, wait * 2); retryAt = now() + wait; }
                failed = true;
              } else { failed = false; wait = FALLBACK; retryAt = 0; }
              // A group command may preempt an unrelated background GET. It
              // is not a room failure: retry the retired read after the command
              // lane is released, even if no further speaker event arrives.
              if (!ok && usable && remaining?.length === 0) {
                if (scope?.targets && !full) {
                  for (const target of scope.targets) targets.add(target);
                  pending = true;
                } else if (scope?.recovery) { failed = true; retryAt = now() + COALESCE; }
              }
              // Refreshing a room does not break event continuity in the rest
              // of the house. Actual disconnect/reset still invalidates all.
              notify(connection, connected && usable);
              if (ok && usable) notify(observed, scope);
            }
            if (user && !current) { explicit = pending = full = true; targets.clear(); }
          }
          if (pending) arm();
          else if (!demo && (!connected || failed)) arm(failed ? Math.max(0, retryAt - now()) : FALLBACK);
        }
      }
    }
    function suspend() {
      if (!active) return;
      active = false; ++generation; ++lifetime;
      pending = full = explicit = connected = false; targets.clear();
      clear();
      if (feed) feed.close();
      feed = null;
      notify(connection, false); notify(invalidate, true);
    }
    return {
      resume() {
        if (active || disposed) return;
        active = true; ++lifetime;
        open(); request();
      },
      suspend,
      refresh(user = false) { if (active) { open(); request(user); } },
      dispose() { suspend(); disposed = true; },
    };
  }
  return { create, COALESCE, FALLBACK, MAX_BACKOFF, EVENTS };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardLive;
