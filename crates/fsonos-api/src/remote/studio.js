// Organize the existing remote. Navigation never dispatches a speaker command.
'use strict';
const StudioRemote = (() => {
  const workspaces = Object.freeze({
    listen: ['GOOD MUSIC. EVERY ROOM.', 'Your house.', 'In harmony.', 'A soundtrack that follows you. A DJ that gets you. You’re in control.'],
    music: ['MORE THAN A PLAY BUTTON.', 'Your taste.', 'In the lead.', 'Find a favorite, choose the next chapter, or let your personal DJ take it from here.'],
    house: ['ONE HOUSE. EVERY GENERATION.', 'Bring your rooms', 'together.', 'Move the music, balance the sound, and keep S1 and S2 households clearly in view.'],
    routines: ['MAKE SPACE FOR THE MOMENT.', 'A little less effort.', 'A little more atmosphere.', 'Scenes, sleep timers, gentle fades, and announcements that give the music back.'],
    activity: ['CONFIDENCE, NOT GUESSWORK.', 'Know what changed.', 'Stay in control.', 'Review the action history and recovery details. Undo is offered only where the daemon supports it.'],
  });
  const tools = Object.freeze([
    { id: 'dj-steering', workspace: 'music', match: /steer/i, title: 'Shape the next DJ pick', description: 'Mood, energy, artists, genres, eras, and whole-work constraints.', keywords: 'whole work classical composer focus dinner next mood' },
    { id: 'dj-session', workspace: 'music', match: /dj session/i, title: 'See why the DJ chose it', description: 'The current work, upcoming selections, and the reasons behind them.', keywords: 'reason explain upcoming movement classical' },
    { id: 'dj-preferences', workspace: 'music', match: /preference|taste/i, title: 'Teach the DJ your taste', description: 'Standing preferences, favorites, avoids, and explicit-content choices.', keywords: 'pin ban artist genre explicit' },
    { id: 'music-library', workspace: 'music', match: /music library/i, title: 'Find music in your library', description: 'Browse and search your music and household favorites.', keywords: 'search spotify album playlist track favorite' },
    { id: 'source-player', workspace: 'music', match: /source|link or stream/i, title: 'Play or queue a link', description: 'Spotify tracks, albums, playlists, or a direct audio stream.', keywords: 'paste url queue next end radio' },
    { id: 'spotify', workspace: 'music', match: /spotify|sync|library/i, title: 'Connect and refresh your library', description: 'Account connection and the library the DJ learns from.', keywords: 'sign in saved likes sync refresh' },
    { id: 'room-groups', workspace: 'house', match: /group|move.*music|transfer/i, title: 'Move music or group rooms', description: 'Move a listening session, copy playback, or bring a household together.', keywords: 'party everywhere s1 s2 coordinator move follow' },
    { id: 'sound-copy', workspace: 'house', match: /copy.*sound|sound.*copy/i, title: 'Copy a speaker’s sound', description: 'Review and copy tone settings to selected rooms.', keywords: 'bass treble loudness sound copy' },
    { id: 'equalizer', workspace: 'house', match: /speaker sound|equaliz/i, title: 'Tune a speaker', description: 'Per-room bass, treble, and loudness.', keywords: 'eq tone bass treble loudness' },
    { id: 'play-mode', workspace: 'house', match: /play.*mode|shuffle|repeat/i, title: 'Change shuffle and repeat', description: 'Control how the selected group plays its queue.', keywords: 'shuffle repeat queue' },
    { id: 'fades', workspace: 'routines', match: /fade/i, title: 'Ease the volume down', description: 'Native volume ramps and a gentle group pause.', keywords: 'quiet softer fade pause' },
    { id: 'scenes', workspace: 'routines', match: /scene/i, title: 'Set the atmosphere with a scene', description: 'A named arrangement of groups, levels, sources, and DJ mood.', keywords: 'dinner work atmosphere preset save apply' },
    { id: 'sleep', workspace: 'routines', match: /sleep|timer/i, title: 'Fall asleep to the music', description: 'Let the daemon fade and pause your group at bedtime.', keywords: 'sleep bedtime quiet goodnight countdown' },
    { id: 'schedules', workspace: 'routines', match: /schedule|routine/i, title: 'Make a listening routine', description: 'Schedule a DJ session, scene, volume change, or pause.', keywords: 'alarm morning weekday time schedule' },
    { id: 'announcements', workspace: 'routines', match: /announce|chime|speech/i, title: 'Say something to the house', description: 'Speech or chimes, with the previous music restored afterward.', keywords: 'dinner ready voice say announcement chime' },
    { id: 'history', workspace: 'activity', match: /history|action|undo/i, title: 'Review changes and available undo', description: 'The daemon’s action history, outcomes, and supported recovery.', keywords: 'undo reverse log audit history recovery' },
  ]);
  function identify(node) {
    const label = [node.id, node.getAttribute('aria-label'), node.querySelector('summary')?.textContent]
      .filter(Boolean).join(' ');
    return tools.find(tool => tool.match.test(label)) || { id: 'other', workspace: 'house', title: label || 'More controls', description: 'Native house controls.', keywords: '' };
  }
  function search(query, available) {
    const words = String(query).toLocaleLowerCase().trim().split(/\s+/).filter(Boolean);
    return available.filter(tool => words.every(word => `${tool.title} ${tool.description} ${tool.keywords}`.toLocaleLowerCase().includes(word)));
  }
  const key = zone => JSON.stringify([zone.household, zone.coordinator_room]);
  function choose(zones, selected) {
    return zones.some(zone => key(zone) === selected) ? selected
      : zones.length ? key(zones.find(zone => zone.transport_state === 'playing') || zones[0]) : null;
  }

  function mount({ document: doc, window: win, house, register, el,
    schedule = callback => win.requestAnimationFrame(callback), cancel = id => win.cancelAnimationFrame(id) }) {
    const $ = id => doc.getElementById(id);
    if (!$('studio-content') || !$('households')) return null;
    const native = $('studio-native'), rail = $('studio-room-rail');
    const panels = new Map(), roomButtons = new Map(), decorated = new WeakSet();
    let workspace = 'listen', selected = null, frame = null, disposed = false, opener = null;
    const on = [];
    const listen = (node, event, fn) => { node.addEventListener(event, fn); on.push(() => node.removeEventListener(event, fn)); };
    const text = (node, value) => { if (node && node.textContent !== value) node.textContent = value; };
    const hidden = (node, value) => { if (node && node.hidden !== value) node.hidden = value; };
    function enqueue() { if (!disposed && frame === null) frame = schedule(() => { frame = null; reconcile(); }); }
    function decorate(parts) {
      if (decorated.has(parts.node)) return;
      decorated.add(parts.node);
      if (parts.art?.parentNode) {
        const sleeve = el('div', { class: 'studio-sleeve' }, el('span', { text: 'ARTWORK UNAVAILABLE', 'aria-hidden': 'true' }));
        parts.art.before(sleeve); sleeve.append(parts.art);
      }
      parts.play?.parentNode?.classList.add('studio-transport');
      parts.panelHost?.classList.add('studio-cardpanels');
      parts.favorite?.classList.add('studio-favorite');
      parts.djRow?.classList.add('studio-dj-controls');
      parts.djPlaying?.classList.add('studio-dj-feedback');
    }
    function reconcile() {
      if (disposed) return;
      // Native modules can mount after us. Move their roots, never recreate the
      // controls or listeners. Unknown sections remain reachable in House.
      for (const node of [...native.children]) {
        if (node.id === 'households' || node.tagName !== 'SECTION') continue;
        const tool = identify(node);
        panels.set(node, tool);
        $('studio-' + tool.workspace).append(node);
      }
      const zones = house.zones || [];
      selected = choose(zones, selected);
      const present = new Set();
      for (const zone of zones) {
        const id = key(zone); present.add(id);
        let button = roomButtons.get(id);
        if (!button) {
          button = el('button', { type: 'button' }, el('span', { class: 'studio-room-icon', 'aria-hidden': 'true', text: '◉' }), el('strong'), el('small'));
          button.addEventListener('click', () => { selected = id; reconcile(); });
          roomButtons.set(id, button); rail.append(button);
        }
        const parts = house.cards.get(id);
        const name = zone.members.join(' + ');
        const status = parts?.lastState?.transport_state || zone.transport_state || 'unknown';
        const title = parts?.lastState?.track?.title;
        text(button.children[1], name);
        text(button.children[2], `${zone.household} · ${status}${title ? ` · ${title}` : ''}`);
        button.setAttribute('aria-pressed', String(id === selected));
        button.setAttribute('aria-label', `${name}, household ${zone.household}, ${status}`);
        if (parts?.node) { decorate(parts); hidden(parts.node, id !== selected); }
      }
      for (const [id, button] of roomButtons) if (!present.has(id)) { button.remove(); roomButtons.delete(id); }
      for (const section of $('households').children) {
        if (section.tagName === 'SECTION') hidden(section, ![...section.querySelectorAll('article.zone')].some(node => !node.hidden));
      }
      const rooms = zones.reduce((count, zone) => count + zone.members.length, 0);
      const homes = new Set(zones.map(zone => zone.household)).size;
      text($('studio-house-count'), rooms ? `${rooms} rooms · ${zones.length} groups · ${homes} ${homes === 1 ? 'household' : 'households'}` : 'Waiting for speaker discovery');
      const live = $('live').textContent === 'Live';
      if (doc.body.dataset.live !== String(live)) doc.body.dataset.live = String(live);
      for (const [node] of panels) if (!node.isConnected) panels.delete(node);
      for (const button of doc.querySelectorAll('[data-tool]')) {
        button.disabled = ![...panels].some(([node, tool]) => tool.id === button.dataset.tool && !node.hidden);
      }
      doc.dispatchEvent(new win.CustomEvent('fsonos:studio-selection', { detail: { key: selected, workspace } }));
    }
    function navigate(next, focus = true) {
      if (!Object.hasOwn(workspaces, next)) return false;
      workspace = next;
      for (const node of doc.querySelectorAll('[data-studio-workspace]')) hidden(node, node.dataset.studioWorkspace !== next);
      for (const button of $('studio-nav').querySelectorAll('button')) {
        if (button.dataset.workspace === next) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
      }
      const [kicker, title, accent, description] = workspaces[next];
      text($('studio-kicker'), kicker);
      $('studio-title').replaceChildren(doc.createTextNode(title + ' '), el('em', { text: accent }));
      text($('studio-description'), description); text($('studio-location'), next.toUpperCase());
      try { win.history.replaceState(null, '', '#studio/' + next); } catch (_) { /* Navigation works without history. */ }
      if (focus) { $('studio-content').focus({ preventScroll: true }); win.scrollTo({ top: 0, behavior: 'instant' }); }
      enqueue(); return true;
    }
    function available() {
      const found = new Map();
      for (const [node, tool] of panels) if (!node.hidden && !found.has(tool.id)) found.set(tool.id, tool);
      return [...found.values()];
    }
    function openTool(id) {
      const found = [...panels].find(([node, tool]) => tool.id === id && !node.hidden);
      if (!found) return false;
      const [node, tool] = found;
      navigate(tool.workspace, false);
      const details = node.matches('details') ? node : node.querySelector('details');
      if (details) details.open = true;
      schedule(() => {
        if (disposed || !node.isConnected) return;
        node.scrollIntoView({ block: 'start', behavior: 'instant' });
        (details?.querySelector('summary') || node.querySelector('button,input,select'))?.focus({ preventScroll: true });
      });
      return true;
    }
    function results() {
      const entries = search($('studio-search').value, available());
      $('studio-results').replaceChildren();
      for (const tool of entries) {
        const button = el('button', { type: 'button' }, el('strong', { text: tool.title }), el('small', { text: tool.description }));
        button.addEventListener('click', () => { $('studio-palette').close(); openTool(tool.id); });
        $('studio-results').append(button);
      }
      if (!entries.length) $('studio-results').append(el('p', { class: 'empty', text: 'No matching control. Try a feature name such as “DJ” or “volume”.' }));
    }
    function palette() {
      const dialog = $('studio-palette');
      if (dialog.open) { dialog.close(); return; }
      opener = doc.activeElement;
      $('studio-search').value = ''; results(); dialog.showModal(); $('studio-search').focus();
    }
    for (const button of $('studio-nav').querySelectorAll('button')) listen(button, 'click', () => navigate(button.dataset.workspace));
    for (const button of doc.querySelectorAll('[data-tool]')) listen(button, 'click', () => openTool(button.dataset.tool));
    listen($('studio-command'), 'click', palette);
    listen($('studio-palette-close'), 'click', () => $('studio-palette').close());
    listen($('studio-palette'), 'close', () => { opener?.focus?.({ preventScroll: true }); });
    listen($('studio-search'), 'input', results);
    listen(doc, 'keydown', event => {
      if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === 'k') { event.preventDefault(); palette(); }
      if (!$('studio-palette').open || !['ArrowDown', 'ArrowUp'].includes(event.key)) return;
      const buttons = [...$('studio-results').querySelectorAll('button')];
      if (!buttons.length) return;
      event.preventDefault(); const at = buttons.indexOf(doc.activeElement);
      buttons[(at + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length].focus();
    });
    function theme(value) {
      doc.body.dataset.theme = value === 'light' ? 'light' : 'dark';
      $('studio-theme').setAttribute('aria-label', `Switch to ${value === 'light' ? 'dark' : 'light'} appearance`);
    }
    try { theme(win.localStorage.getItem('fsonos.studio.theme')); } catch (_) { theme('dark'); }
    listen($('studio-theme'), 'click', () => {
      const next = doc.body.dataset.theme === 'light' ? 'dark' : 'light'; theme(next);
      try { win.localStorage.setItem('fsonos.studio.theme', next); } catch (_) { /* Private browsing still works. */ }
    });
    const observer = new win.MutationObserver(enqueue);
    observer.observe(native, { childList: true, subtree: true, characterData: true });
    observer.observe($('live'), { childList: true, characterData: true, subtree: true });
    register(() => ({ node: el('span', { hidden: true }), observe: enqueue, stale: enqueue, block: enqueue, dispose: enqueue }));
    reconcile();
    navigate(win.location.hash.startsWith('#studio/') ? win.location.hash.slice(8) : 'listen', false);
    return { reconcile, navigate, openTool, available, selected: () => selected,
      dispose() { disposed = true; observer.disconnect(); if (frame !== null) cancel(frame); for (const off of on) off(); } };
  }
  return { mount, identify, search, choose, key, tools, workspaces };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = StudioRemote;
else if (typeof house !== 'undefined' && typeof registerCardPanel === 'function') {
  StudioRemote.mount({ document, window, house, register: registerCardPanel, el });
}
