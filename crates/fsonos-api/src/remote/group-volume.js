// Whole-group levels use the native policy-checked volume command, not N room writes.
'use strict';
const GroupVolumeRemote = (() => {
  const level = (v) => Number.isInteger(v) && v >= 0 && v <= 100;
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const problem = (e) => [e?.message || 'The request failed.', e?.hint].filter(Boolean).join(' ');
  function shape(zone) {
    if (!zone || !text(zone.household) || !text(zone.coordinator_room)
        || !Array.isArray(zone.members) || zone.members.length < 2 || !zone.members.every(text)
        || !zone.members.includes(zone.coordinator_room) || new Set(zone.members).size !== zone.members.length) {
      throw new Error('Choose a group containing at least two rooms.');
    }
    return JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]);
  }
  function observed(value, group) {
    if (!value || shape(value.zone) !== group) throw new Error('The group changed. Refresh rooms before adjusting it.');
    if (!level(value.group_volume)) throw new Error('The group level is unavailable. Refresh after the coordinator answers.');
    return value.group_volume;
  }
  // Pending and uncertain results outlive cards. An explicit read of one split
  // group cannot clear its siblings or a different operation still in progress.
  function recovery() {
    const records = new Set(), listeners = new Set();
    const overlaps = (record, keys) => keys.some(k => record.keys.has(k));
    const notify = (keys) => { for (const fn of listeners) { try { fn(keys); } catch (_) { /* View only. */ } } };
    return {
      status(keys) {
        const matches = [...records].filter(r => overlaps(r, keys));
        return { pending: matches.some(r => r.pending), uncertain: matches.some(r => !r.pending) };
      },
      start(keys) {
        if ([...records].some(r => overlaps(r, keys))) return null;
        const record = { keys: new Set(keys), pending: true };
        records.add(record); notify(keys); return record;
      },
      finish(record, uncertain) {
        if (!records.has(record)) return;
        if (uncertain) record.pending = false; else records.delete(record);
        notify([...record.keys]);
      },
      clear(keys) {
        for (const record of records) {
          if (record.pending) continue;
          for (const key of keys) record.keys.delete(key);
          if (!record.keys.size) records.delete(record);
        }
        notify(keys);
      },
      subscribe(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    };
  }
  const shared = recovery();
  function model({ zone, call, changed = () => {}, current = () => true,
    blocked = () => false, acquire = () => () => {}, externalRefresh = false, registry = shared }) {
    const group = shape(zone), name = `${zone.coordinator_room}@${zone.household}`;
    const keys = zone.members.map(room => JSON.stringify([zone.household, room]));
    let disposed = false, active = true, serial = 0, epoch = 0;
    let value = null, fresh = false, reading = false, writing = false, review = null, result = null;
    let message = 'Waiting for the group level.';
    const live = () => !disposed && active && current();
    const ready = () => live() && fresh && !reading && !writing && !blocked()
      && !registry.status(keys).pending && !registry.status(keys).uncertain;
    const view = () => Object.freeze({ value, fresh: fresh && live(), reading, writing, review, result,
      ready: ready(), ...registry.status(keys), message });
    const emit = () => { if (live()) { try { changed(view()); } catch (_) { /* View only. */ } } };
    function stale(detail = 'Group level is stale; waiting for a fresh observation.') {
      ++serial; ++epoch; reading = false; fresh = false; review = null; message = detail; emit();
    }
    const unsubscribe = registry.subscribe(affected => {
      if (!disposed && affected.some(k => keys.includes(k))) emit();
    });
    function observe(dto) {
      if (!live() || writing || registry.status(keys).pending) return false;
      ++serial; reading = false;
      try {
        const next = observed(dto, group);
        if (next !== value) { review = null; ++epoch; }
        value = next; fresh = true; message = ''; emit(); return true;
      } catch (error) { stale(problem(error)); return false; }
    }
    async function refresh(explicit = true) {
      if (!live() || reading || writing || blocked() || registry.status(keys).pending) return false;
      stale('Reading the group level…'); const version = serial;
      reading = true; emit();
      try {
        const next = observed(await call('state', { params: { room: name } }), group);
        if (!live() || serial !== version) return false;
        value = next; fresh = true; reading = false; message = '';
        if (explicit) registry.clear(keys);
        emit(); return true;
      } catch (error) { if (live() && serial === version) stale(problem(error)); return false; }
      finally { if (serial === version) { reading = false; emit(); } }
    }
    function preview(wanted) {
      if (!live() || writing) return null;
      review = null; ++epoch;
      if (!ready() || !level(wanted) || wanted === value) { emit(); return null; }
      review = Object.freeze({ volume: wanted, before: value }); result = null; emit(); return review;
    }
    function cancelReview() { if (live() && !writing) { review = null; ++epoch; emit(); } }
    async function apply(selected) {
      if (!selected || selected !== review || !ready()) return false;
      const version = epoch;
      const valid = () => live() && epoch === version;
      review = null; writing = true; ++serial; reading = false; result = null;
      let lease = null, operation = null, sent = false, success = false;
      message = 'Checking the group and its current level…'; emit();
      try {
        if (!valid()) return false;
        lease = acquire();
        if (typeof lease !== 'function') throw new Error('Another room-level or playback change is still running. Review again after it finishes.');
        operation = registry.start(keys);
        if (!operation) throw new Error('This group has an outstanding volume result. Refresh its level first.');
        if (!valid()) return false;
        const before = observed(await call('state', { params: { room: name } }), group);
        if (!valid()) return false;
        if (before !== selected.before) throw new Error('The group level changed. Read its new level and choose again.');
        message = 'Applying the reviewed group level…'; emit();
        if (!valid()) return false;
        sent = true;
        const answer = await call('volume', { body: { zone: name, group: true, volume: selected.volume } });
        if (!answer || !text(answer.done) || typeof answer.changed !== 'boolean' || !level(answer.volume)
            || (answer.notes != null && (!Array.isArray(answer.notes) || !answer.notes.every(n => n && typeof n.detail === 'string')))) {
          throw new Error('The group-volume response is incomplete.');
        }
        success = true;
        if (!disposed) result = Object.freeze({ status: 'accepted', volume: answer.volume,
          detail: [answer.done, ...(answer.notes || []).map(n => n.detail)].join(' ') });
      } catch (error) {
        if (!disposed) result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent', detail: `${problem(error)} ${sent
          ? 'The change may be partial or still finish. Refresh the group level before another adjustment; it was not repeated.'
          : 'No group-volume command was sent.'}` });
      } finally {
        fresh = false; review = null; ++epoch; ++serial; reading = false; writing = false;
        message = success ? 'Accepted; waiting for an observed group level.' : 'Read the group level before another adjustment.';
        if (operation) registry.finish(operation, sent && !success);
        if (typeof lease === 'function') lease();
        emit();
        if (success && live() && !externalRefresh) await refresh(false);
      }
      return success;
    }
    return { view, observe, stale, refresh, preview, cancelReview, apply, block: emit,
      suspend() { if (!disposed) { stale('Group controls paused while the page is hidden.'); active = false; } },
      resume() { if (!disposed) { active = true; stale(); } },
      dispose() { disposed = true; active = false; ++epoch; ++serial; review = null; unsubscribe(); },
    };
  }

  function panel({ zone, call, el, ...options }) {
    const heading = el('strong', { text: 'Group volume' });
    const shown = el('output', { text: '–' });
    const range = el('input', { type: 'range', min: 0, max: 100, step: 1, value: 0, disabled: true,
      'aria-label': `Group volume for ${zone.members.join(' + ')}` });
    const refresh = el('button', { type: 'button', text: 'Refresh group level' });
    const review = el('div', { class: 'row', hidden: true });
    const detail = el('p', { role: 'status', 'aria-live': 'polite' });
    const help = el('p', { class: 'group-volume-help', text: 'Changes every room in this group through the daemon. Room caps and quiet hours still apply; playback and mute are unchanged.' });
    const node = el('section', { class: 'group-volume', 'aria-label': 'Whole-group volume' },
      heading, el('div', { class: 'group-volume-level' }, range, shown), review, detail, refresh, help);
    let consent = null;
    const control = model({ zone, call, ...options, changed(state) {
      range.disabled = !state.ready;
      range.value = String(state.review?.volume ?? state.value ?? 0);
      shown.textContent = state.review ? `${state.review.volume} (not sent)` : state.value === null ? '–' : `${state.value}${state.fresh ? '' : ' (stale)'}`;
      range.setAttribute('aria-valuetext', shown.textContent);
      refresh.disabled = state.reading || state.writing || state.pending || Boolean(options.blocked?.());
      node.setAttribute('aria-busy', String(state.reading || state.writing || state.pending));
      const notice = state.result?.detail || (state.uncertain
        ? 'An earlier group-volume result is uncertain. Refresh this group level before changing it.' : state.message);
      if (detail.textContent !== notice) detail.textContent = notice;
      if (consent !== state.review) {
        consent = state.review; review.replaceChildren(); review.hidden = !consent;
        if (consent) {
          const chosen = consent;
          review.append(el('button', { type: 'button', text: `Set group to ${chosen.volume}`,
            onclick: () => { void control.apply(chosen); } }),
          el('button', { type: 'button', text: 'Cancel group change', onclick: () => control.cancelReview() }));
        }
      }
      for (const child of review.children) child.disabled = !state.ready;
    } });
    range.addEventListener('input', () => control.preview(Number(range.value)));
    range.addEventListener('keydown', event => { if (event.key === 'Escape') control.cancelReview(); });
    refresh.addEventListener('click', () => { void control.refresh(); });
    control.block();
    return { node, ...control };
  }

  // The same volumeFlights map already fences room sliders across card changes.
  // Reserve every member before the group write; never queue a second room write
  // behind an unknown group outcome or drain an old slider draft automatically.
  function attach(parts, env) {
    const addresses = parts.zone.members.map(room => `${room}@${parts.zone.household}`);
    const entries = () => [...env.cards.values()].flatMap(card => [...card.sliders.values()])
      .filter(entry => addresses.includes(entry.room));
    const busy = () => parts.writing || env.transportBusy(parts) || addresses.some(room => env.volumeFlights.has(room))
      || entries().some(entry => entry.timer !== null || entry.pendingVolume !== null);
    const acquire = () => {
      if (!env.ownsCard(parts) || busy() || !env.beginAction(parts)) return null;
      const owner = {};
      for (const room of addresses) env.volumeFlights.set(room, owner);
      for (const entry of entries()) { ++entry.version; entry.input.disabled = true; }
      let released = false;
      return () => {
        if (released) return; released = true;
        for (const room of addresses) if (env.volumeFlights.get(room) === owner) env.volumeFlights.delete(room);
        for (const entry of entries()) ++entry.version;
        env.endAction(parts);
      };
    };
    const result = panel({ zone: parts.zone, call: env.call, el: env.el, registry: env.registry || shared,
      acquire, blocked: busy, current: () => env.ownsCard(parts), externalRefresh: true });
    const visibility = () => { if (env.document.hidden) result.suspend(); else result.resume(); };
    const hide = () => result.suspend(), show = () => { if (!env.document.hidden) result.resume(); };
    env.document.addEventListener('visibilitychange', visibility);
    env.window.addEventListener('pagehide', hide); env.window.addEventListener('pageshow', show);
    if (env.document.hidden) result.suspend();
    const dispose = result.dispose;
    result.dispose = () => {
      env.document.removeEventListener('visibilitychange', visibility);
      env.window.removeEventListener('pagehide', hide); env.window.removeEventListener('pageshow', show);
      dispose();
    };
    return result;
  }
  return { model, panel, attach, recovery, shape, observed };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = GroupVolumeRemote;
if (typeof registerCardPanel === 'function') {
  registerCardPanel(parts => parts.zone.members.length > 1
    ? GroupVolumeRemote.attach(parts, { get cards() { return house.cards; }, volumeFlights,
      ownsCard, transportBusy, beginAction, endAction, call, el, document, window })
    : { node: el('span', { hidden: true }), observe() {} });
}
