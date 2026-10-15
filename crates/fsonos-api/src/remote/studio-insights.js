// A selected group's real DJ story. Reads only; native controls own every write.
'use strict';
const StudioInsights = (() => {
  function model({ read, validate, changed = () => {}, now = Date.now }) {
    let target = null, generation = 0, active = true, disposed = false;
    let flight = null, queued = false;
    const state = { target: null, value: null, at: null, loading: false, stale: true, error: '' };
    const emit = () => { if (!disposed) { try { changed(state); } catch (_) { /* Rendering cannot strand a read. */ } } };
    async function drain() {
      // Yield once so the single-flight handle is installed before a sync error.
      await Promise.resolve();
      while (queued && active && !disposed && target) {
        queued = false;
        const selected = target, version = generation;
        state.loading = true; state.error = ''; emit();
        try {
          const value = validate(await read(selected), selected);
          if (disposed || !active || version !== generation) continue;
          state.value = value; state.at = now(); state.stale = false;
        } catch (error) {
          if (!disposed && active && version === generation) {
            state.stale = true;
            state.error = [error?.message || 'The DJ could not be read.', error?.hint].filter(Boolean).join(' ');
          }
        } finally {
          if (!disposed && version === generation) { state.loading = false; emit(); }
        }
      }
    }
    function refresh() {
      if (disposed || !active || !target) return Promise.resolve(false);
      queued = true;
      if (!flight) flight = drain().finally(() => { flight = null; if (queued && active && !disposed) void refresh(); });
      return flight;
    }
    function select(next) {
      if (disposed || (target?.signature === next?.signature && target?.target === next?.target)) return;
      target = next ? Object.freeze({ ...next }) : null; ++generation;
      state.target = target; state.value = null; state.at = null; state.error = '';
      state.stale = true; state.loading = false; queued = false; emit();
      if (target) void refresh();
    }
    return { state, select, refresh,
      stale() { if (!disposed) { ++generation; state.loading = false; state.stale = true; emit(); } },
      visible(value) {
        if (disposed || active === value) return;
        active = value; ++generation; queued = false; state.loading = false; state.stale = true; emit();
        if (active && target) void refresh();
      },
      dispose() { disposed = true; active = false; ++generation; queued = false; },
    };
  }

  function attach({ document: doc, window: win, house, register, schema, call, el }) {
    const host = doc.getElementById('studio-native');
    if (!host || !schema) return null;
    let selectedKey = null, selectedShape = null, activeWorkspace = 'listen', disposed = false;
    const trackKeys = new WeakMap();
    let dirty = false;
    const heading = el('h2', {}, 'Not random. ', el('em', { text: 'Chosen for you.' }));
    const updated = el('span', { class: 'studio-story-time' });
    const refresh = el('button', { type: 'button', text: 'Refresh DJ', 'aria-label': 'Refresh the selected group’s DJ story' });
    const content = el('div', { class: 'studio-story-content' });
    const notice = el('p', { class: 'studio-story-notice', role: 'status', 'aria-live': 'polite' });
    const node = el('section', { class: 'studio-story', 'aria-label': 'Your personal DJ, now and next' },
      el('header', {}, el('div', {}, el('p', { class: 'studio-kicker', text: 'YOUR DJ / PERSONAL BY DESIGN' }), heading),
        el('div', { class: 'studio-story-actions' }, updated, refresh)), notice, content);
    host.after(node);
    function openSteering() {
      doc.querySelector('.studio-paths [data-tool="dj-steering"]')?.click();
    }
    function work(item, index) {
      const description = [item.composer, item.minutes == null ? '' : `${item.minutes} min`,
        item.movements > 1 ? `${item.movements} movements · kept together` : ''].filter(Boolean).join(' · ');
      return el('li', {}, el('span', { class: 'studio-next-number', text: String(index + 1).padStart(2, '0'), 'aria-hidden': 'true' }),
        el('div', {}, el('strong', { text: item.title }), el('small', { text: description })));
    }
    function render(state) {
      if (disposed) return;
      node.hidden = !state.target;
      node.setAttribute('aria-busy', String(state.loading));
      refresh.disabled = state.loading || !state.target || activeWorkspace !== 'listen';
      node.setAttribute('aria-label', state.target ? `DJ story for ${state.target.label}` : 'Your personal DJ, now and next');
      updated.textContent = state.at === null ? '' : `${state.stale ? 'Previous observation' : 'Observed'} ${new Date(state.at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`;
      notice.textContent = state.error || (state.loading ? 'Reading the selected group’s DJ…' : state.stale && state.value ? 'The house changed. Refresh DJ for the latest selection.' : '');
      content.replaceChildren();
      if (!state.value) {
        content.append(el('p', { class: 'empty', text: 'The DJ’s reasoning and upcoming works appear here when its state is available.' })); return;
      }
      const value = state.value;
      if (value.problem) content.append(el('p', { class: 'studio-story-problem', text: value.problem }));
      if (!value.running) {
        content.append(el('div', { class: 'studio-story-current' }, el('h3', { text: 'A set shaped around you.' }),
          el('p', { text: 'The DJ is not running in this group. Use its Start DJ control above, or shape a mood before the next session.' }),
          el('button', { type: 'button', text: 'Explore DJ steering ↗', onclick: openSteering })));
        return;
      }
      const current = el('div', { class: 'studio-story-current' });
      if (value.now) {
        const item = value.now;
        const badge = item.movements > 1 ? item.movement ? `WHOLE WORK · MOVEMENT ${item.movement} OF ${item.movements}` : `WHOLE WORK · ${item.movements} MOVEMENTS` : 'WHY THIS SELECTION';
        current.append(el('span', { class: 'studio-work-badge', text: badge }), el('h3', { text: item.title }),
          el('p', { class: 'studio-story-credit', text: [item.composer, item.album].filter(Boolean).join(' · ') }),
          el('p', { class: 'studio-story-reason', text: item.reason.summary }));
        if (item.movements > 1 && item.movement) current.append(el('progress', { value: item.movement, max: item.movements, 'aria-label': `Movement ${item.movement} of ${item.movements}` }));
        if (item.reason.relaxed?.length) current.append(el('p', { class: 'studio-story-problem', text: `Broadened to find a pick: ${item.reason.relaxed.join('; ')}.` }));
      } else current.append(el('h3', { text: 'The next selection is taking shape.' }), el('p', { text: 'No current DJ work is reported yet.' }));
      const source = { session: 'Your session steering', program: 'Time-of-day program', none: 'No active steering' }[value.steering.source] || value.steering.source;
      current.append(el('div', { class: 'studio-steering-line' }, el('span', { text: [source, value.steering.mood].filter(Boolean).join(' · ') }), el('button', { type: 'button', text: 'Shape what’s next ↗', onclick: openSteering })));
      const next = el('div', { class: 'studio-story-next' }, el('p', { class: 'studio-kicker', text: 'UP NEXT / THE DJ’S PLAN' }));
      if (value.next?.length) next.append(el('ol', {}, ...value.next.slice(0, 3).map(work)));
      else next.append(el('p', { class: 'empty', text: 'No upcoming work is reported yet.' }));
      next.append(el('p', { class: 'studio-next-note', text: 'Planned selections, not a promise of playback. Steering can change the next pick.' }));
      content.append(current, next);
    }
    const control = model({ read: zone => call('sessionStatus', { params: { room: zone.target } }), validate: schema.statusOf, changed: render });
    function selection(detail) {
      selectedKey = detail.key; activeWorkspace = detail.workspace;
      const zone = house.zones.find(each => JSON.stringify([each.household, each.coordinator_room]) === selectedKey);
      let target = null;
      try { target = zone ? schema.zonesOf([zone])[0] : null; } catch (_) { /* Only complete groups can be read. */ }
      if (selectedShape !== target?.signature) dirty = false;
      selectedShape = target?.signature;
      control.visible(!doc.hidden && activeWorkspace === 'listen');
      control.select(target);
      render(control.state);
    }
    const selected = event => selection(event.detail || {});
    const visibility = () => control.visible(!doc.hidden && activeWorkspace === 'listen');
    const hide = () => control.visible(false);
    const changed = event => { if (event.detail?.zone === control.state.target?.target) void control.refresh(); };
    doc.addEventListener('fsonos:studio-selection', selected);
    doc.addEventListener('fsonos:dj-changed', changed);
    doc.addEventListener('visibilitychange', visibility);
    win.addEventListener('pagehide', hide); win.addEventListener('pageshow', visibility);
    refresh.addEventListener('click', () => { void control.refresh(); });
    register(parts => ({ node: el('span', { hidden: true }),
      observe(dto) {
        const identity = JSON.stringify([dto.track?.uri, dto.track?.queue_position]);
        const previous = trackKeys.get(parts); trackKeys.set(parts, identity);
        if (JSON.stringify([parts.zone.household, parts.zone.coordinator_room]) !== selectedKey) return;
        let target;
        try { target = schema.zonesOf([parts.zone])[0]; } catch (_) { control.stale(); return; }
        if (target.signature !== selectedShape) selection({ key: selectedKey, workspace: activeWorkspace });
        else if (dirty || (previous !== undefined && previous !== identity)) { dirty = false; void control.refresh(); }
      },
      stale() {
        if (JSON.stringify([parts.zone.household, parts.zone.coordinator_room]) === selectedKey) { dirty = true; control.stale(); }
      },
      dispose() { if (JSON.stringify([parts.zone.household, parts.zone.coordinator_room]) === selectedKey) control.select(null); },
    }));
    const initial = [...house.cards.values()].find(parts => !parts.node.hidden);
    selection({ key: initial ? JSON.stringify([initial.zone.household, initial.zone.coordinator_room]) : null,
      workspace: doc.querySelector('#studio-nav [aria-current="page"]')?.dataset.workspace || 'listen' });
    return { node, control, dispose() { disposed = true; control.dispose(); doc.removeEventListener('fsonos:studio-selection', selected);
      doc.removeEventListener('fsonos:dj-changed', changed); doc.removeEventListener('visibilitychange', visibility);
      win.removeEventListener('pagehide', hide); win.removeEventListener('pageshow', visibility); } };
  }
  return { model, attach };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = StudioInsights;
else if (typeof house !== 'undefined' && typeof DjSessionRemote !== 'undefined') {
  StudioInsights.attach({ document, window, house, register: registerCardPanel, schema: DjSessionRemote, call, el });
}
