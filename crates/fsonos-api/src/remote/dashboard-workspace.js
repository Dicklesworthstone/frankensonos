// Interaction layer: room scope, real queue inspection and keyboard navigation.
'use strict';
const DashboardWorkspace = (() => {
  function moveFocus(event, nodes, input) {
    if (event.altKey || event.ctrlKey || event.metaKey || event.isComposing) return;
    const list = [...nodes].filter((n) => !n.disabled && n.getClientRects().length);
    if (!list.length) return;
    const index = list.indexOf(event.target);
    let next;
    if (event.key === 'ArrowDown') next = index < 0 ? 0 : (index + 1) % list.length;
    else if (event.key === 'ArrowUp') next = index < 0 ? list.length - 1 : (index + list.length - 1) % list.length;
    else if (index >= 0 && event.key === 'Home') next = 0;
    else if (index >= 0 && event.key === 'End') next = list.length - 1;
    else return;
    if (index < 0 && event.target !== input) return;
    event.preventDefault(); list[next].focus();
  }
  function create({ call, controller, session, el, icon, openSearch, native, immerse }) {
    const $ = (id) => document.getElementById(id);
    let active = true, immersive = false, chosenTab = 'picks', roomKey = '';
    const queue = DashboardQueue.model({ call, changed: paintQueue });
    const queueEditor = DashboardQueue.editor({ queue, controller, session, el });
    $('queue-items').after(queueEditor.node);
    const library = typeof DashboardLibraryPanel !== 'undefined'
      ? DashboardLibraryPanel.create({ call, controller, session }) : null;
    const activity = typeof DashboardActivityPanel !== 'undefined'
      ? DashboardActivityPanel.create({ call, controller, session }) : null;
    function paintQueue(state) {
      const p = state.page, container = $('queue-items'), scroll = container.scrollTop;
      const focus = container.contains(document.activeElement) ? document.activeElement?.dataset?.queueFocus : null;
      queueEditor.resetRows();
      container.replaceChildren(); container.setAttribute('aria-busy', String(state.loading));
      $('queue-status').textContent = state.error || (state.loading ? 'Reading the speaker queue…'
        : !p ? 'Waiting for a fresh listening group.' : state.stale ? 'Last observation · refresh to update'
          : `${p.total} tracks · ${p.playing ? `playing #${p.playing}` : 'not playing from queue'}`);
      for (const row of p?.items || []) {
        const li = el('li');
        if (p.playing === row.position) li.setAttribute('aria-current', 'true');
        const copy = el('div'); copy.append(el('strong', '', row.title || 'Untitled track'), el('small', '', row.creator || row.album));
        li.append(el('span', 'queue-position', p.playing === row.position ? '▶' : row.position), copy, queueEditor.rowControls(row, p)); container.append(li);
      }
      if (p && !p.total) container.append(el('li', 'empty', 'The speaker queue is empty. Choose music from your collection.'));
      queueEditor.update();
      if (focus) [...container.querySelectorAll('[data-queue-focus]')].find((n) => n.dataset.queueFocus === focus && !n.disabled)?.focus({ preventScroll: true });
      container.scrollTop = scroll;
      $('queue-page').textContent = p?.total ? `${p.offset + 1}–${p.offset + p.items.length} of ${p.total}` : '';
      queueNavigation();
    }
    function queueNavigation() {
      const state = queue.state, p = state.page;
      $('queue-prev').disabled = controller.state.busy || !p || state.stale || state.loading || !p.offset;
      $('queue-next').disabled = controller.state.busy || !p || state.stale || state.loading || p.offset + p.items.length >= p.total;
      $('queue-refresh').disabled = controller.state.busy || state.loading || !controller.selected()?.fresh;
    }
    function tab(name, focus = false) {
      if (!['picks', 'queue'].includes(name)) return;
      chosenTab = name;
      for (const id of ['picks', 'queue']) {
        const b = $(`tab-${id}`), selected = id === name;
        b.setAttribute('aria-selected', String(selected)); b.tabIndex = selected ? 0 : -1;
        $(id === 'picks' ? 'dj-picks-panel' : 'speaker-queue-panel').hidden = !selected;
        if (selected && focus) b.focus();
      }
      if (name === 'queue' && active && !immersive) queue.open(); else queue.close();
    }
    for (const [index, name] of ['picks', 'queue'].entries()) {
      const b = $(`tab-${name}`);
      b.addEventListener('click', () => tab(name));
      b.addEventListener('keydown', (e) => {
        if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
        e.preventDefault(); tab(e.key === 'Home' ? 'picks' : e.key === 'End' ? 'queue' : index ? 'picks' : 'queue', true);
      });
    }
    $('queue-refresh').addEventListener('click', () => { void queue.read(); });
    $('queue-prev').addEventListener('click', () => queue.navigate(-1));
    $('queue-next').addEventListener('click', () => queue.navigate(1));
    $('dock-queue').addEventListener('click', () => { tab('queue'); $('queue-title').scrollIntoView({ block: 'center', behavior: 'auto' }); $('tab-queue').focus({ preventScroll: true }); });
    function paintRooms() {
      if (!$('rooms-dialog').open) return;
      const needle = $('room-filter').value.trim().toLocaleLowerCase();
      const rows = controller.state.items.filter((i) => [i.zone.household, ...i.zone.members].join(' ').toLocaleLowerCase().includes(needle));
      const focused = document.activeElement?.dataset?.choice;
      $('room-choices').replaceChildren();
      for (const i of rows) {
        const z = i.zone, target = DashboardCore.key(z), signature = DashboardCore.shape(z);
        const b = el('button', 'room-choice'); b.type = 'button'; b.dataset.choice = target; b.disabled = controller.state.busy;
        b.setAttribute('aria-pressed', String(target === controller.state.selected));
        const copy = el('div'); copy.append(el('strong', '', z.coordinator_room), el('small', '', `${z.household} · ${z.members.length > 1 ? z.members.join(' + ') : 'Independent room'}`));
        b.append(icon('speaker'), copy, el('span', 'room-state', i.fresh ? i.state?.transport_state || 'unknown' : 'stale'));
        if (target === controller.state.selected) b.append(icon('check'));
        b.addEventListener('click', () => {
          const current = controller.state.items.find((i) => DashboardCore.key(i.zone) === target);
          if (!active || controller.state.busy || !current || DashboardCore.shape(current.zone) !== signature) { paintRooms(); return; }
          session.cancel();
          if (controller.select(target)) $('rooms-dialog').close();
        }); $('room-choices').append(b);
      }
      if (!rows.length) $('room-choices').append(el('p', 'empty', controller.state.items.length ? 'No matching rooms. Try another room or household.' : 'No rooms discovered yet. Refresh the dashboard or check the full remote.'));
      if (focused) [...$('room-choices').querySelectorAll('button')].find((b) => b.dataset.choice === focused)?.focus({ preventScroll: true });
    }
    function rooms() {
      if (!active || immersive || controller.state.busy || document.querySelector('dialog[open]')) return;
      $('room-filter').value = ''; $('rooms-dialog').showModal(); paintRooms(); $('room-filter').focus();
    }
    for (const b of document.querySelectorAll('[data-open-rooms]')) b.addEventListener('click', rooms);
    for (const b of document.querySelectorAll('[data-open-search]')) b.addEventListener('click', openSearch);
    $('close-rooms').addEventListener('click', () => $('rooms-dialog').close());
    $('room-filter').addEventListener('input', paintRooms);
    $('rooms-dialog').addEventListener('keydown', (e) => {
      if (e.key === 'Escape') { e.preventDefault(); $('rooms-dialog').close(); }
      else moveFocus(e, $('room-choices').querySelectorAll('button'), $('room-filter'));
    });
    $('search-dialog').addEventListener('keydown', (e) => {
      if (e.key === 'Escape') { e.preventDefault(); $('search-dialog').close(); }
      else moveFocus(e, $('search-results').querySelectorAll('button'), $('search-input'));
    });
    $('shortcut-help').addEventListener('click', () => $('shortcuts-dialog').showModal());
    $('close-shortcuts').addEventListener('click', () => $('shortcuts-dialog').close());
    document.addEventListener('keydown', (e) => {
      if (!active || immersive || e.defaultPrevented || e.repeat || e.isComposing || e.ctrlKey || e.metaKey || e.altKey
          || document.querySelector('dialog[open]') || e.target.closest?.('input,textarea,select,button,a,summary,[contenteditable]')) return;
      if (e.key.toLowerCase() === 'r') { e.preventDefault(); rooms(); }
      else if (e.key.toLowerCase() === 'v') { e.preventDefault(); immerse(true); }
      else if (e.key === ' ' && controller.selected()?.state?.track) { e.preventDefault(); void native(controller.selected()?.state?.transport_state === 'playing' ? 'pause' : 'resume'); }
      else if (e.key === '?') { e.preventDefault(); $('shortcuts-dialog').showModal(); }
    });
    function volume(open) {
      $('toggle-volume').setAttribute('aria-expanded', String(open));
      document.querySelector('.player-dock').classList.toggle('volume-open', open);
    }
    $('toggle-volume').addEventListener('click', () => volume($('toggle-volume').getAttribute('aria-expanded') !== 'true'));
    document.addEventListener('pointerdown', (e) => {
      if (!e.target.closest?.('#volume-panel,#toggle-volume')) volume(false);
    });
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && $('toggle-volume').getAttribute('aria-expanded') === 'true') {
        volume(false); $('toggle-volume').focus(); e.preventDefault();
      }
    });
    const nav = [...document.querySelectorAll('.sidebar nav a[href^="#"]')];
    for (const link of nav) link.addEventListener('click', () => {
      for (const other of nav) { other.classList.toggle('active', other === link); if (other === link) other.setAttribute('aria-current', 'location'); else other.removeAttribute('aria-current'); }
    });
    // Native Escape restores the opener for these read-only dialogs.
    for (const id of ['rooms-dialog', 'shortcuts-dialog']) $(id).addEventListener('click', (e) => {
      if (e.target !== $(id)) return;
      const r = $(id).getBoundingClientRect();
      if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) $(id).close();
    });
    return {
      observe(state) {
        queue.observe(controller.selected());
        queueEditor.update();
        queueNavigation();
        library?.observe();
        activity?.observe();
        const next = JSON.stringify([state.selected, state.busy, state.items.map((i) => [i.zone, i.fresh, i.state?.transport_state])]);
        if (next !== roomKey) { roomKey = next; paintRooms(); }
        for (const b of document.querySelectorAll('[data-open-rooms]')) b.disabled = state.busy;
      },
      invalidate() { queue.invalidate(); activity?.invalidate(); },
      immerse(value) { immersive = value; if (value) { queue.suspend(); activity?.suspend(); } else if (active) { queue.resume(); activity?.resume(); if (chosenTab === 'queue') queue.open(); } },
      suspend() { active = false; library?.suspend(); activity?.suspend(); queue.suspend(); for (const id of ['rooms-dialog', 'shortcuts-dialog']) if ($(id).open) $(id).close(); },
      resume() { active = true; library?.resume(); if (!immersive) { queue.resume(); activity?.resume(); } },
    };
  }
  return { create, moveFocus };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardWorkspace;
