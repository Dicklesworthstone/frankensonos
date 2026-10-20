// One event-driven snapshot lane for the Listening Room; never sends commands.
'use strict';
const DashboardLive = (() => {
  const COALESCE = 350, FALLBACK = 15000, MAX_BACKOFF = 60000;
  const EVENTS = ['zone.state', 'topology.changed', 'player.health', 'action.logged', 'dj.status'];
  function create({ connect = null, reload, invalidate = () => {}, connection = () => {},
    blocked = () => false, retryAfter = () => 0, observed = () => {}, demo = false,
    schedule = setTimeout, cancel = clearTimeout, now = () => globalThis.performance?.now() ?? Date.now() }) {
    let active = false, disposed = false, busy = false, connected = false;
    let pending = false, explicit = false, failed = false, generation = 0, lifetime = 0;
    let feed = null, timer = null, timerAt = 0, wait = FALLBACK, notBefore = 0, retryAt = 0;
    const clear = () => { if (timer !== null) cancel(timer); timer = null; };
    const notify = (fn, ...args) => { try { fn(...args); } catch (_) { /* Views do not own delivery. */ } };
    function arm(delay = COALESCE) {
      if (!active || busy || disposed) return;
      const due = Math.max(now() + delay, notBefore, retryAt);
      if (!Number.isFinite(due)) { clear(); return; }
      const at = Math.min(due, now() + MAX_BACKOFF);
      if (timer !== null && timerAt <= at) return;
      clear(); timerAt = at;
      timer = schedule(() => { timer = null; void run(); }, Math.max(0, at - now()));
    }
    function request(user = false, hard = false) {
      if (!active || disposed) return;
      ++generation; pending = true; explicit ||= user;
      notify(invalidate, hard);
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
      for (const name of EVENTS) next.addEventListener(name, () => { if (current()) request(); });
      for (const name of ['events.reset', 'reset']) next.addEventListener(name, () => {
        if (current()) { notify(connection, false); request(false, true); }
      });
    }
    async function run() {
      if (!active || disposed || busy) return;
      if (now() < Math.max(notBefore, retryAt)) { arm(0); return; }
      if (blocked()) { pending = true; arm(1000); return; }
      open();
      const version = generation, life = lifetime, user = explicit;
      explicit = pending = false; busy = true;
      notify(connection, false);
      let ok = false, minimum = 0;
      try { ok = await reload(user) === true; minimum = retryAfter(); }
      catch (error) { minimum = error?.retryAfter || 0; }
      finally {
        // A server minimum remains binding across hide/show and reconnects.
        if (typeof minimum === 'number' && minimum > 0) notBefore = Math.max(notBefore, now() + minimum);
        busy = false;
        if (active && !disposed) {
          if (version === generation && life === lifetime) {
            failed = !ok;
            wait = ok ? FALLBACK : Math.min(MAX_BACKOFF, wait * 2);
            retryAt = ok ? 0 : now() + wait;
            notify(connection, connected && ok);
            if (ok) notify(observed);
          } else if (user && life === lifetime) explicit = true;
          if (pending) arm();
          else if (!demo && (!connected || failed)) arm(wait);
        }
      }
    }
    function suspend() {
      if (!active) return;
      active = false; ++generation; ++lifetime;
      pending = explicit = connected = false;
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
