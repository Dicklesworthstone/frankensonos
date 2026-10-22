// A listening cockpit for FrankenSonos, not a second playback backend.
'use strict';
(() => {
  const C = DashboardCore, S = DashboardSession, $ = (id) => document.getElementById(id);
  const demo = new URLSearchParams(location.search).get('demo') === '1' || document.body.dataset.preview === 'true';
  const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
  function icon(name) {
    const n = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    const u = document.createElementNS(n.namespaceURI, 'use'); u.setAttribute('href', `#i-${name}`); n.append(u); n.setAttribute('aria-hidden', 'true'); return n;
  }
  const sentence = (v) => String(v).replaceAll('_', ' ').replaceAll('-', ' ');
  const title = (v) => sentence(v).replace(/^./, (c) => c.toUpperCase());
  const setting = (key, value) => { try { if (value === undefined) return localStorage.getItem(`fsonos:visual:${key}`); localStorage.setItem(`fsonos:visual:${key}`, value); } catch (_) { /* Storage is optional. */ } };
  // Explicit preview fixtures: no fallback ever substitutes these for a failed daemon read.
  const fixtureZones = [
    { household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen', 'Dining room'], transport_state: 'playing' },
    { household: 'S2', coordinator_room: 'Terrace', members: ['Terrace'], transport_state: 'paused' },
    { household: 'S1', coordinator_room: 'Reading room', members: ['Reading room'], transport_state: 'stopped' },
  ];
  const fixtureWork = { composer: 'Adrian Vale', title: 'Nocturne in Blue', album: 'Rooms of Memory', performers: ['Northlight Ensemble'],
    movements: 3, movement: 2, movement_title: 'II. Glass & Rain', minutes: 24,
    reason: { summary: 'A gentler turn. An artist you haven’t heard lately, and space for a whole work to unfold.',
      factors: [{ factor: 'composer_spacing', weight: 1600 }, { factor: 'energy_fit', weight: 1280 }, { factor: 'feedback', weight: 1150 }, { factor: 'taste', weight: 1100 }], relaxed: [] } };
  const fixtureNext = [
    { composer: 'Mira Sol', title: 'Kind of Somewhere', album: 'Side Streets', performers: [], movements: 1, minutes: 5, reason: { summary: 'A change of artist and texture.', factors: [], relaxed: [] } },
    { composer: 'Canopy', title: 'The Long Way Home', album: 'Open Windows', performers: [], movements: 1, minutes: 8, reason: { summary: 'A familiar favorite, not a recent repeat.', factors: [], relaxed: [] } },
  ];
  const fixtureScenes = [
    { name: 'Dinner', saved: 1700000000, groups: [{ coordinator: 'Studio@S2', members: ['Kitchen@S2', 'Dining room@S2'], source: { kind: 'dj', mood: 'dinner' }, playing: true }], volumes: { 'Studio@S2': 24, 'Kitchen@S2': 20, 'Dining room@S2': 22 }, mutes: {} },
    { name: 'Deep focus', saved: 1700000001, groups: [{ coordinator: 'Studio@S2', members: [], source: { kind: 'dj', mood: 'focus' }, playing: true }], volumes: { 'Studio@S2': 18 }, mutes: {} },
    { name: 'Slow Sunday', saved: 1700000002, groups: [{ coordinator: 'Reading room@S1', members: [], source: { kind: 'dj', mood: 'sunday-morning' }, playing: true }], volumes: { 'Reading room@S1': 15 }, mutes: {} },
  ];
  const fixtureHits = [
    { title: 'Nocturne in Blue', subtitle: 'Adrian Vale · Northlight Ensemble', kind: 'track', source_uri: 'spotify:track:demonocturne' },
    { title: 'Rooms of Memory', subtitle: 'Adrian Vale', kind: 'album', source_uri: 'spotify:album:demomemory' },
    { title: 'Kind of Somewhere', subtitle: 'Mira Sol', kind: 'track', source_uri: 'spotify:track:demosomewhere' },
    { title: 'Evening radio', subtitle: 'Household favorite', kind: 'favorite', favorite: 'FV:2/7' },
  ];
  const request = C.request();
  async function call(url, body) {
    if (!demo) return request(url, body);
    if (body !== undefined) throw new Error('Design preview never sends speaker commands.');
    if (url === '/zones') return structuredClone(fixtureZones);
    if (url.endsWith('/state')) {
      const zone = fixtureZones.find((z) => url.includes(encodeURIComponent(C.key(z))));
      const index = fixtureZones.indexOf(zone);
      return { zone, transport_state: zone.transport_state, volume: 22, group_volume: 24, mute: false,
        track: index === 0 ? { uri: 'demo:movement2', title: 'II. Glass & Rain', creator: 'Adrian Vale · Northlight Ensemble', album: 'Rooms of Memory', position_secs: 154, duration_secs: 486, queue_position: 2 }
          : index === 1 ? { uri: 'demo:terrace', title: 'After Hours', creator: 'Mira Sol', album: 'Side Streets', position_secs: 54, duration_secs: 273 } : null };
    }
    if (url.includes('/queue?')) {
      const zone = fixtureZones.find((z) => url.includes(encodeURIComponent(C.key(z))));
      const offset = Number(new URLSearchParams(url.split('?')[1]).get('offset'));
      const titles = ['I. Still Water', 'II. Glass & Rain', 'III. First Light', 'Kind of Somewhere', 'The Long Way Home'];
      return { zone: zone.coordinator_room, total: titles.length, playing: zone === fixtureZones[0] ? 2 : null,
        revision: 'fictional-queue', items: titles.slice(offset, offset + 40).map((title, n) => ({ position: offset + n + 1,
          title, creator: offset + n < 3 ? 'Adrian Vale · Northlight Ensemble' : offset + n === 3 ? 'Mira Sol' : 'Canopy', uri: `demo:queued${offset + n}` })) };
    }
    if (url.endsWith('/dj')) {
      const zone = fixtureZones.find((z) => url.includes(encodeURIComponent(C.key(z))));
      const running = zone === fixtureZones[0];
      return { zone: zone.coordinator_room, running, now: running ? fixtureWork : null, next: running ? fixtureNext : [],
        steering: { source: running ? 'session' : 'program', mood: running ? 'focus' : 'calm', constraints: { energy_bias: -1 }, added: {}, expires_at: null, expires_in_secs: null } };
    }
    if (url === '/dj/moods') return { moods: ['focus', 'dinner', 'calm', 'bright', 'sunday-morning'].map((name) => ({ name, constraints: { energy_bias: name === 'bright' ? 1 : -1 } })) };
    if (url === '/scenes') return fixtureScenes;
    if (url.startsWith('/scenes/')) return fixtureScenes.find((s) => decodeURIComponent(url.split('/')[2]) === s.name);
    if (url.startsWith('/library/search')) {
      const q = new URLSearchParams(url.split('?')[1]).get('q').toLowerCase();
      return fixtureHits.filter((h) => `${h.title} ${h.subtitle}`.toLowerCase().includes(q));
    }
    if (url.startsWith('/history')) return ['The Space Between', 'Soft Landing', 'Weightless', 'Another Morning'].map((name, i) => ({ title: name,
      artist: ['Canopy', 'Mira Sol', 'Northlight Ensemble', 'Adrian Vale'][i], source_uri: `demo:history${i}`, played_at: Math.floor(Date.now() / 1000) - (i + 1) * 420 }));
    throw new Error('No preview fixture for this request.');
  }
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let active = !document.hidden, disposed = false, immersive = false, lastFocus = null, motionPaused = reduced.matches;
  let artKey = '', visualKey = '', roomKey = '', mapKey = '', sessionKey = '', sceneKey = '', reviewed = null, selectedKey = '';
  let workspace, controller, session, live, clockTimer = null;
  let reviewReading = false, searchTimer = null, historyVersion = 0, historyRunning = false, catalogsDirty = true;
  const renderer = DashboardVisualizer.create($('visualizer'), { reduced: reduced.matches });
  const audio = DashboardAudio.create({ changed(s) {
    $('microphone').textContent = s.status === 'live' ? 'Stop microphone' : s.status === 'requesting' ? 'Cancel microphone' : 'Use microphone';
    $('microphone').setAttribute('aria-pressed', String(s.status === 'live'));
    $('audio-status').textContent = s.message;
    $('visual-source').textContent = s.status === 'live' ? 'LIVE MICROPHONE · ANALYZED LOCALLY' : 'AMBIENT ART · NOT MEASURED AUDIO';
    renderer.audio(s.status === 'live' ? audio.sample : null);
  } });
  controller = C.model({ call, demo, changed: render });
  session = S.model({ call, controller, demo, changed: renderSession });
  workspace = DashboardWorkspace.create({ call, controller, session, el, icon, openSearch,
    native: (...args) => native(...args), immerse });
  const resize = () => { const box = $('stage').getBoundingClientRect(); renderer.resize(box.width, box.height, devicePixelRatio || 1); };
  const observer = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(resize) : null;
  observer?.observe($('stage')); window.addEventListener('resize', resize);
  function theme(value) {
    document.documentElement.dataset.theme = value;
    $('theme').setAttribute('aria-label', `Switch to ${value === 'dark' ? 'light' : 'dark'} appearance`);
    $('theme').querySelector('use').setAttribute('href', value === 'dark' ? '#i-sun' : '#i-moon');
    setting('theme', value);
  }
  theme(setting('theme') === 'light' ? 'light' : 'dark');
  $('theme').addEventListener('click', () => theme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark'));
  function chooseMode(value) {
    if (!DashboardVisualizer.MODES.includes(value)) return;
    renderer.mode(value); setting('mode', value);
    for (const b of document.querySelectorAll('[data-mode]')) b.setAttribute('aria-pressed', String(b.dataset.mode === value));
  }
  for (const b of document.querySelectorAll('[data-mode]')) b.addEventListener('click', () => chooseMode(b.dataset.mode));
  chooseMode(setting('mode') || 'aurora');
  for (const id of ['art', 'dock-art']) {
    $(id).addEventListener('load', () => { if ($(id).getAttribute('src') === artKey) $(id).hidden = false; });
    $(id).addEventListener('error', () => { $(id).hidden = true; });
  }
  function motion() {
    renderer.pause(motionPaused); $('motion').textContent = !renderer.supported ? 'Canvas unavailable' : motionPaused ? 'Resume motion' : 'Pause motion';
    $('motion').setAttribute('aria-pressed', String(motionPaused)); $('motion').disabled = !renderer.supported;
  }
  $('motion').addEventListener('click', () => { motionPaused = !motionPaused; motion(); });
  reduced.addEventListener('change', () => { motionPaused = reduced.matches; motion(); });
  $('microphone').addEventListener('click', () => ['live', 'requesting'].includes(audio.state.status) ? audio.stop() : void audio.start());
  function immerse(value) {
    if (value === immersive) return;
    immersive = value; if (value) lastFocus = document.activeElement;
    $('stage').classList.toggle('immersive', value); document.body.classList.toggle('is-immersive', value); $('exit-immerse').hidden = !value;
    for (const n of document.querySelectorAll('.sidebar,.topbar,.greeting,.demo-banner,#summary,#error,#room-section,.intelligence-column,.lower-grid,.recent-section,.page-footer,.player-dock,.outcome,.mobile-nav')) n.inert = value;
    workspace?.immerse(value); renderer.immersive(value); resize();
    if (value) {
      $('exit-immerse').focus();
      try { Promise.resolve($('stage').requestFullscreen?.()).catch(() => {}); } catch (_) { /* Full-window view remains. */ }
    } else {
      if (document.fullscreenElement) document.exitFullscreen?.().catch(() => {});
      lastFocus?.focus();
    }
  }
  $('immerse').addEventListener('click', () => immerse(true)); $('nav-immerse').addEventListener('click', () => immerse(true));
  $('exit-immerse').addEventListener('click', () => immerse(false));
  document.addEventListener('fullscreenchange', () => { if (immersive && !document.fullscreenElement) immerse(false); });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && immersive) { e.preventDefault(); immerse(false); }
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k' && !immersive && !document.querySelector('dialog[open]')) { e.preventDefault(); openSearch(); }
    if (immersive && e.key === 'Tab') {
      const nodes = [...$('stage').querySelectorAll('button:not(:disabled), summary')].filter((n) => !n.hidden && n.getClientRects().length);
      if (e.shiftKey && document.activeElement === nodes[0]) { e.preventDefault(); nodes.at(-1)?.focus(); }
      else if (!e.shiftKey && document.activeElement === nodes.at(-1)) { e.preventDefault(); nodes[0]?.focus(); }
    }
  });
  for (const b of document.querySelectorAll('[data-house-view]')) b.addEventListener('click', () => {
    $('rooms').hidden = b.dataset.houseView !== 'rooms'; $('topology').hidden = b.dataset.houseView !== 'map';
    for (const n of document.querySelectorAll('[data-house-view]')) n.setAttribute('aria-pressed', String(n === b));
  });
  function controls() {
    const item = controller.selected(), can = controller.canControl(), playing = item?.state?.transport_state === 'playing';
    const hasTrack = !!item?.state?.track;
    for (const id of ['play', 'previous', 'next']) $(id).disabled = !can || !hasTrack;
    $('play-icon').firstElementChild.setAttribute('href', playing ? '#i-pause' : '#i-play');
    $('play').setAttribute('aria-label', `${playing ? 'Pause' : 'Resume'} ${item?.zone.members.join(' + ') || 'selected group'}`);
    const volume = item?.zone.members.length === 1 ? item?.state?.volume : item?.state?.group_volume;
    $('group-volume').textContent = Number.isInteger(volume) ? volume : '—';
    $('volume-down').disabled = $('volume-up').disabled = !can || !Number.isInteger(volume);
    for (const b of document.querySelectorAll('[data-command]')) {
      b.disabled = !can || !hasTrack;
      if (b.dataset.command === 'toggle') { b.querySelector('use').setAttribute('href', playing ? '#i-pause' : '#i-play'); b.setAttribute('aria-label', $('play').getAttribute('aria-label')); const label = b.querySelector('[data-transport-label]'); if (label) label.textContent = playing ? 'Pause' : 'Play'; }
    }
    const d = session?.state, sessionReady = d?.dj && d.target === controller.state.selected && !d.loading && !reviewReading && active && (demo || can);
    for (const id of ['like', 'dislike', 'skip-work']) $(id).disabled = !sessionReady || !d.dj.running || !d.dj.now;
    $('dj-start-stop').disabled = !sessionReady; $('dj-clear').disabled = !sessionReady || !d.dj.running;
    for (const b of document.querySelectorAll('.mood-chip')) b.disabled = !sessionReady;
    for (const b of document.querySelectorAll('.scene-card')) b.disabled = reviewReading || !active || (!demo && !can);
  }
  function render(state) {
    if (disposed) return;
    $('connection').textContent = demo ? 'Read-only design preview' : !state.active ? 'Suspended while hidden' : state.error ? 'Some state is unavailable' : state.connected ? 'Live from your daemon' : state.initialized ? 'Snapshot · reconnecting' : 'Connecting to your daemon';
    $('connection').dataset.connected = String(state.connected && !state.error && !demo);
    $('refresh').disabled = state.busy || state.loading;
    $('error').hidden = !state.error; $('error').textContent = state.error;
    $('outcome').textContent = state.message; $('outcome').hidden = !state.message; $('stage-outcome').textContent = state.message;
    const count = state.items.reduce((n, i) => n + i.zone.members.length, 0), playing = state.items.filter((i) => i.fresh && i.state?.transport_state === 'playing').length;
    $('nav-count').textContent = state.initialized ? count : '—';
    const facts = [[count, 'rooms discovered', 'house'], [playing, 'groups playing', 'wave'], [new Set(state.items.map((i) => i.zone.household)).size, 'households', 'speaker']];
    $('summary').replaceChildren(...facts.map(([n, label, symbol]) => { const row = el('span'); row.append(icon(symbol), el('strong', '', state.initialized ? n : '—'), label); return row; }));
    const item = controller.selected(), track = item?.state?.track;
    const selected = state.selected;
    $('scope').textContent = item ? `${item.zone.members.join(' + ')} · ${item.zone.household}` : 'Choose a room';
    $('scope').title = $('scope').textContent;
    $('stage-status').textContent = demo ? 'PREVIEW SESSION' : !item?.fresh ? 'WAITING FOR FRESH STATE' : item.state.transport_state.toUpperCase();
    $('target').textContent = item ? `${item.zone.coordinator_room}${item.zone.members.length > 1 ? ` + ${item.zone.members.length - 1}` : ''} · ${item.zone.household}` : 'Pick a group to begin';
    $('dock-title').textContent = track?.title || 'Your next listen is waiting'; $('dock-target').textContent = item ? `${item.zone.members.join(' + ')} · ${item.zone.household}` : 'Choose a listening group';
    $('dock-title').title = $('dock-title').textContent; $('dock-target').title = $('dock-target').textContent;
    const art = !demo && track ? `/art?${new URLSearchParams({ zone: selected, v: C.hash(`${track.uri}|${track.title}`).toString(36) })}` : '';
    if (art !== artKey) { artKey = art; for (const id of ['art', 'dock-art']) { $(id).hidden = true; if (art) $(id).src = art; else $(id).removeAttribute('src'); } }
    const visual = `${selected}|${track?.uri}|${item?.fresh && item?.state?.transport_state}`;
    if (visual !== visualKey) { visualKey = visual; renderer.track(C.hash(track?.uri || selected), item?.fresh && item?.state?.transport_state === 'playing'); }
    renderRooms(state); renderMap(state); renderHero(); timing(); controls(); workspace?.observe(state);
    if (selected !== selectedKey) { selectedKey = selected; sessionKey = ''; queueMicrotask(() => { if (!disposed && active) void session.load(); }); }
  }
  function renderHero() {
    const item = controller.selected(), track = item?.state?.track;
    const d = session?.state, current = d?.target === controller.state.selected ? d.dj?.now : null;
    // A delayed DJ response must not relabel another controller's source.
    const whole = current && current.movements > 1 && typeof current.movement_title === 'string'
      && current.movement_title === track?.title;
    $('hero-kicker').textContent = whole ? `A WHOLE WORK · ${current.minutes ? `${current.minutes} MINUTES` : 'IN SEQUENCE'}` : track ? 'ON YOUR SPEAKERS' : 'YOUR NEXT GREAT LISTEN';
    $('track-title').textContent = whole ? current.title : track?.title || (controller.state.initialized ? 'A little room for music.' : 'Let the music find you.');
    $('track-title').title = $('track-title').textContent;
    $('track-by').textContent = whole ? current.composer : track?.creator || 'Your taste. Your next discovery.';
    $('track-album').textContent = whole ? current.performers?.join(' · ') || current.album || 'The complete recording' : track?.album || 'Pick from your collection to begin';
    $('whole-work').hidden = !whole;
    if (whole) {
      $('work-detail').textContent = current.movement_title || track?.title || 'Movement title not reported';
      $('work-count').textContent = `${current.movement || '—'} / ${current.movements}`;
      const pieces = Math.min(current.movements, 32);
      $('movements').replaceChildren(...Array.from({ length: pieces }, (_, i) => el('span', `movement${i + 1 < current.movement ? ' done' : i + 1 === current.movement ? ' current' : ''}`)));
      $('movements').setAttribute('aria-label', `Movement ${current.movement || 'unknown'} of ${current.movements}. Every movement plays in order.`);
    }
  }
  function renderRooms(state) {
    const key = JSON.stringify([state.selected, state.busy, state.items.map((i) => [i.zone, i.fresh, i.state?.transport_state, i.state?.track?.title])]);
    if (key === roomKey) return; roomKey = key;
    const focus = document.activeElement?.dataset?.room;
    $('rooms').replaceChildren();
    for (const item of state.items) {
      const z = item.zone, b = el('button', 'room-card'); b.type = 'button'; b.dataset.room = C.key(z); b.disabled = state.busy;
      b.setAttribute('aria-pressed', String(C.key(z) === state.selected)); b.setAttribute('aria-label', `Select ${z.members.join(' + ')} in ${z.household}`);
      const top = el('div', 'room-top'), symbol = el('span', 'room-icon'); symbol.append(icon('speaker'));
      top.append(symbol, el('span', `room-state${item.fresh ? '' : ' stale'}`, item.fresh ? item.state.transport_state : 'stale'));
      b.append(top, el('strong', '', z.coordinator_room), el('p', '', item.state?.track?.title || 'Nothing playing'),
        el('p', 'room-meta', `${z.household} · ${z.members.length === 1 ? 'Independent room' : `${z.members.length} linked rooms`}${z.degraded.length ? ' · Bonded player missing' : ''}`));
      b.addEventListener('click', () => { session.cancel(); controller.select(C.key(z)); }); $('rooms').append(b);
    }
    if (!state.items.length) $('rooms').append(el('p', 'empty', state.initialized ? 'No speakers discovered. Check the speaker LAN or open the full remote for diagnosis.' : 'Discovering the house…'));
    if (focus) [...$('rooms').querySelectorAll('[data-room]')].find((b) => b.dataset.room === focus)?.focus({ preventScroll: true });
  }
  function renderMap(state) {
    const key = JSON.stringify([state.busy, state.selected, state.items.map((i) => [i.zone, i.fresh])]);
    if (key === mapKey) return; mapKey = key;
    $('topology').replaceChildren();
    for (const item of state.items) {
      const row = el('div', 'map-row'), group = el('div', 'map-group');
      for (const name of item.zone.members) {
        const n = el('span', `map-node${name === item.zone.coordinator_room ? ' leader' : ''}${!item.fresh ? ' stale' : ''}`);
        n.append(icon('speaker'), name); group.append(n);
      }
      const select = el('button', 'map-select', 'Listen here →'); select.disabled = state.busy;
      select.addEventListener('click', () => { session.cancel(); controller.select(C.key(item.zone)); }); group.append(select);
      row.append(el('span', 'home-label', item.zone.household), group); $('topology').append(row);
    }
    $('topology').append(el('p', 'section-note', state.items.length ? 'Highlighted nodes coordinate each group. Dashed outlines mean stale observations. S1 and S2 remain separate households.' : 'The map will appear after speaker discovery.'));
  }
  function renderSession(state) {
    if (disposed) return;
    const key = JSON.stringify([state.target, state.dj, state.error]);
    if (key !== sessionKey) {
      sessionKey = key;
      const d = state.dj;
      $('dj-insight').replaceChildren(); $('factor-list').replaceChildren(); $('relaxed').replaceChildren(); $('next-works').replaceChildren();
      $('dj-badge').textContent = !d ? 'STATE UNKNOWN' : d.running ? 'SESSION ACTIVE' : 'READY WHEN YOU ARE';
      $('dj-start-stop').textContent = d?.running ? 'Stop DJ' : 'Start DJ';
      if (d?.now) {
        $('dj-insight').append(el('span', 'insight-work', `${d.now.composer} · ${d.now.title}`), el('p', 'insight-quote', d.now.reason.summary));
        for (const f of (d.now.reason.factors || []).slice(0, 6)) {
          const row = el('div', 'factor'); row.dataset.direction = f.weight > 1000 ? 'up' : f.weight < 1000 ? 'down' : 'neutral';
          const label = el('div', 'factor-label'); label.append(el('span', '', title(f.factor)), el('strong', '', `×${(f.weight / 1000).toFixed(2).replace(/0$/, '')}`));
          const bar = el('div', 'factor-track'), p = el('progress'); p.max = 2000; p.value = Math.max(0, Math.min(2000, f.weight));
          p.setAttribute('aria-label', `${title(f.factor)} multiplier ${f.weight / 1000}, neutral is one. Visual scale capped at two.`); bar.append(p); row.append(label, bar); $('factor-list').append(row);
        }
        if (d.now.reason.relaxed?.length) $('relaxed').append(el('p', '', `Relaxed filters: ${d.now.reason.relaxed.join(', ')}. The pool was too small with all filters applied.`));
      } else $('dj-insight').append(el('p', 'insight-quote', state.error || (d?.running ? 'The DJ is keeping the queue fed. No current work is reported yet.' : 'A varied set from your own taste, with complete works and a reason behind every pick. Start the DJ or choose a mood below.')));
      if (d?.problem) $('dj-insight').append(el('p', 'small-note', d.problem));
      (d?.next || []).forEach((w, i) => {
        const row = el('article', 'next-work'), art = el('span', 'work-art'); art.dataset.color = C.hash(w.title) % 4; art.setAttribute('aria-hidden', 'true');
        const copy = el('div', 'next-copy'); copy.append(el('strong', '', w.title), el('span', '', `${w.composer} · ${w.movements > 1 ? `${w.movements} movements, one work` : 'One song'}`));
        row.append(el('span', 'work-number', `0${i + 1}`), art, copy, el('span', 'work-duration', w.minutes ? `${w.minutes} min` : '—')); $('next-works').append(row);
      });
      if (!d?.next?.length) $('next-works').append(el('p', 'empty', d?.running ? 'The next picks have not been reported yet.' : 'Start the DJ to see its next selections. This is the DJ’s plan, not a fabricated queue.'));
      const steering = d?.steering;
      $('steering-detail').textContent = steering ? `${steering.source === 'session' ? 'Your session' : steering.source === 'program' ? 'Time-of-day program' : 'No active steering'}${steering.mood ? ` · ${title(steering.mood)}` : ''}${steering.expires_at ? ` · until ${new Date(steering.expires_at * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}` : ''}. Mood changes shape the next pick, not the work playing.` : 'Read the session to see its current steering.';
      renderHero();
    }
    const catalogs = JSON.stringify([state.moods, state.scenes, state.dj?.steering?.mood]);
    if (catalogs !== sceneKey) {
      sceneKey = catalogs; $('moods').replaceChildren(); $('scenes').replaceChildren();
      for (const mood of state.moods) {
        const b = el('button', 'mood-chip', title(mood.name)); b.dataset.mood = mood.name;
        b.setAttribute('aria-pressed', String(mood.name === state.dj?.steering?.mood));
        b.addEventListener('click', () => { const seconds = $('mood-duration').value; review(state.dj?.running ? 'steer' : 'start', { mood: mood.name, for_secs: seconds ? Number(seconds) : null }); });
        $('moods').append(b);
      }
      if (!state.moods.length) $('moods').append(el('p', 'small-note', 'No mood catalog available. Refresh to read your presets.'));
      for (const s of state.scenes) {
        const b = el('button', 'scene-card'); b.dataset.scene = s.name;
        const g = el('span', 'scene-glyph'); g.append(icon(s.groups.some((g) => g.source.kind === 'dj') ? 'spark' : 'scene'));
        const rooms = new Set([...s.groups.flatMap((g) => [g.coordinator, ...g.members]), ...Object.keys(s.volumes), ...Object.keys(s.mutes)]);
        b.append(g, el('strong', '', s.name), el('span', '', `${rooms.size} named rooms · Review scene`));
        b.addEventListener('click', async () => {
          if (reviewReading) return;
          reviewReading = true; controls();
          try {
            const r = await session.reviewScene(s.name);
            if (!r && active) { $('outcome').hidden = false; $('outcome').textContent = session.state.error || 'The house changed during review. Refresh and choose the scene again.'; }
          } finally { reviewReading = false; controls(); }
        }); $('scenes').append(b);
      }
      if (!state.scenes.length) $('scenes').append(el('p', 'empty', 'Save a scene in the full remote. Your real saved scenes will appear here.'));
    }
    $('catalog-error').textContent = state.catalogError;
    $('scene-result').replaceChildren();
    if (state.result?.kind === 'scene') {
      const r = state.result; $('scene-result').append(el('p', '', r.complete ? 'All reported scene steps completed.' : 'Some scene steps did not complete.'));
      for (const f of r.failed) $('scene-result').append(el('p', '', `Failed: ${Object.entries(f).map(([k, v]) => `${k}: ${typeof v === 'string' ? v : JSON.stringify(v)}`).join(' · ')}`));
      for (const f of r.skipped) $('scene-result').append(el('p', '', `Skipped: ${f}`));
    }
    renderSearch(); renderReview(state.pending); controls();
  }
  async function review(kind, args) {
    if (reviewReading || !active || (!demo && !controller.canControl())) return null;
    const target = controller.state.selected;
    reviewReading = true; controls();
    try {
      // A quiet live connection does not poll the DJ. Read it on demand before
      // constructing a review, rather than letting its age disable controls.
      if (kind !== 'play' && !await session.load()) return null;
      if (!active || target !== controller.state.selected) return null;
      const r = session.review(kind, args);
      if (!r) { $('outcome').hidden = false; $('outcome').textContent = 'Read a fresh group and DJ session before choosing this action. Use Refresh.'; }
      return r;
    } finally { reviewReading = false; controls(); }
  }
  function renderReview(r) {
    const dialog = $('review-dialog');
    if (!r) { reviewed = null; if (dialog.open) dialog.close(); return; }
    if (r === reviewed) return; reviewed = r;
    $('review-title').textContent = r.title; $('review-target').textContent = `${r.label} · ${r.target}`;
    $('review-description').textContent = r.detail; $('review-details').replaceChildren();
    if (r.work) {
      const row = el('div', 'review-spec'); row.append(el('h3', '', r.work.title), el('p', '', `${r.work.composer} · ${r.work.movements} movement${r.work.movements === 1 ? '' : 's'}`)); $('review-details').append(row);
    }
    if (r.mood) {
      $('review-details').append(el('p', 'review-duration', `Mood: ${title(r.mood.name)} · ${r.body.for_secs ? `${r.body.for_secs / 60} minutes` : 'until cleared'}`));
      const row = el('details', 'review-spec'); row.append(el('summary', '', 'Exact mood constraints'), el('pre', '', JSON.stringify(r.mood.constraints, null, 2))); $('review-details').append(row);
    }
    if (r.scene) {
      for (const g of r.scene.groups) {
        const row = el('div', 'review-spec'); row.append(el('h3', '', [g.coordinator, ...g.members].join(' + ')),
          el('p', '', `Source: ${S.sceneSource(g.source)}`),
          el('p', '', g.playing == null ? 'Keep playback state' : g.playing ? 'Playing' : 'Paused or stopped')); $('review-details').append(row);
      }
      const levels = el('div', 'review-spec'); levels.append(el('h3', '', 'Requested room levels (policy can clamp)'));
      for (const [name, value] of Object.entries(r.scene.volumes)) levels.append(el('p', '', `${name}: ${value}`));
      for (const [name, value] of Object.entries(r.scene.mutes)) levels.append(el('p', '', `${name}: ${value ? 'muted' : 'unmuted'}`));
      if (!Object.keys(r.scene.volumes).length && !Object.keys(r.scene.mutes).length) levels.append(el('p', '', 'No levels or mutes changed by this scene.'));
      $('review-details').append(levels);
    }
    const cancel = el('button', 'outline-button', 'Keep things as they are'); cancel.addEventListener('click', () => session.cancel());
    const confirm = el('button', 'primary-button', demo ? 'Preview only — no changes' : 'Confirm action'); confirm.id = 'confirm-action'; confirm.disabled = demo;
    confirm.addEventListener('click', () => { void session.confirm(r); });
    $('review-buttons').replaceChildren(cancel, confirm);
    if (!dialog.open) dialog.showModal();
  }
  $('close-review').addEventListener('click', () => session.cancel());
  $('review-dialog').addEventListener('cancel', (e) => { e.preventDefault(); session.cancel(); });
  for (const [id, kind] of [['like', 'like'], ['dislike', 'dislike'], ['skip-work', 'skip'], ['dj-clear', 'clear']]) $(id).addEventListener('click', () => review(kind));
  $('dj-start-stop').addEventListener('click', () => review(session.state.dj?.running ? 'stop' : 'start'));
  function openSearch() {
    if (!$('search-dialog').open) $('search-dialog').showModal();
    $('search-scope').textContent = controller.state.selected ? `Search your cached library and favorites for ${controller.state.selected}.` : 'Select a group first to search its household.';
    $('search-input').focus(); renderSearch();
  }
  for (const id of ['open-library', 'open-library-nav']) $(id).addEventListener('click', openSearch);
  $('close-search').addEventListener('click', () => $('search-dialog').close());
  $('search-form').addEventListener('submit', (e) => { e.preventDefault(); clearTimeout(searchTimer); void session.search($('search-input').value); });
  $('search-input').addEventListener('input', () => { clearTimeout(searchTimer); searchTimer = setTimeout(() => { searchTimer = null; if (active && $('search-dialog').open) void session.search($('search-input').value); }, 300); });
  function renderSearch() {
    if (!$('search-dialog').open) return;
    const s = session.state; $('search-results').replaceChildren();
    if (s.searching) $('search-results').append(el('p', 'empty', 'Searching your collection…'));
    else if (s.searchError) $('search-results').append(el('p', 'empty', s.searchError));
    else if (!s.hits.length) $('search-results').append(el('p', 'empty', s.query ? 'No matches. Try an artist, title or a few different words.' : 'Your tracks, albums, playlists and household favorites, in one place.'));
    for (const h of s.hits) {
      const b = el('button', 'search-result'); b.disabled = !h.playable;
      const copy = el('div'); copy.append(el('strong', '', h.title), el('small', '', h.subtitle)); b.append(icon('library'), copy, el('span', 'hit-kind', h.kind), icon('arrow'));
      b.addEventListener('click', () => { $('search-dialog').close(); review('play', { hit: h }); }); $('search-results').append(b);
    }
  }
  async function history() {
    if (!active || historyRunning) return;
    historyRunning = true; $('history-refresh').disabled = true; const v = ++historyVersion;
    try {
      const rows = await call('/history?limit=4'); if (!active || v !== historyVersion) return;
      if (!Array.isArray(rows) || rows.length > 4 || rows.some((r) => !r || typeof r.source_uri !== 'string' || !Number.isSafeInteger(r.played_at) || Math.abs(r.played_at) > 8640000000000)) throw new Error('Listening history is unreadable.');
      $('history').replaceChildren();
      for (const r of rows) {
        const row = el('article', 'history-item'), art = el('div', 'history-art'), copy = el('div', 'history-copy');
        art.dataset.color = C.hash(r.source_uri) % 4; art.setAttribute('aria-hidden', 'true');
        const date = new Date(r.played_at * 1000), stamp = el('time', '', date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })); stamp.dateTime = date.toISOString(); stamp.title = date.toLocaleString();
        copy.append(el('strong', '', r.title || r.source_uri), el('p', '', r.artist || 'Sonos playback'), stamp); row.append(art, copy); $('history').append(row);
      }
      if (!rows.length) $('history').append(el('p', 'empty', 'No recorded plays yet. Your next chapter starts here.'));
    } catch (e) { if (active && v === historyVersion) $('history').replaceChildren(el('p', 'empty', C.problem(e))); }
    finally { historyRunning = false; $('history-refresh').disabled = false; }
  }
  function timing() {
    const item = controller.selected(), at = C.elapsed(item, performance.now()), duration = item?.state?.track?.duration_secs;
    $('elapsed').textContent = `${item?.fresh && item?.state?.transport_state === 'playing' && at !== null ? '≈' : ''}${C.clock(at)}`;
    $('duration').textContent = C.clock(duration); $('progress').max = duration > 0 ? duration : 1; $('progress').value = at !== null && duration > 0 ? at : 0;
    $('progress').setAttribute('aria-label', at === null ? 'Position unavailable' : `Estimated track position ${C.clock(at)} of ${C.clock(duration)}`);
  }
  live = DashboardLive.create({
    demo,
    connect: () => typeof RemoteEventFeed !== 'undefined' ? RemoteEventFeed.open('/events')
      : typeof EventSource !== 'undefined' ? new EventSource('/events') : null,
    reload: (user) => controller.refresh(user),
    blocked: () => controller.state.busy || controller.state.loading,
    retryAfter: () => controller.state.retryAfter,
    connection: (value) => controller.connection(value),
    invalidate(hard) {
      controller.invalidate(); workspace?.invalidate();
      if (hard) {
        ++historyVersion; catalogsDirty = true;
        session.suspend();
        if (active) session.resume();
        request.cancelReads();
      }
    },
    observed() {
      if (!session.state.loading && !session.state.pending) void session.load();
      if (catalogsDirty) { catalogsDirty = false; void session.catalog(); void history(); }
    },
  });
  function kick(user = false) { if (user) catalogsDirty = true; live.refresh(user); }
  function suspend() {
    active = false; ++historyVersion;
    clearTimeout(clockTimer); clockTimer = null; clearTimeout(searchTimer); searchTimer = null;
    live.suspend(); request.cancelReads();
    audio.stop('Microphone stopped while hidden. Enable it again explicitly to resume.');
    workspace?.suspend(); session.suspend(); controller.suspend(); renderer.visible(false);
  }
  function clockLoop() { if (!active || disposed) return; timing(); controls(); clockTimer = setTimeout(clockLoop, 1000); }
  function resume() {
    if (document.hidden || disposed) return;
    active = true; catalogsDirty = true; workspace?.resume(); controller.resume(); session.resume(); renderer.visible(true); live.resume();
    if (clockTimer === null) clockLoop();
  }
  document.addEventListener('visibilitychange', () => document.hidden ? suspend() : resume());
  window.addEventListener('pagehide', suspend); window.addEventListener('pageshow', () => { if (!active) resume(); });
  window.addEventListener('online', () => { if (active) kick(); });
  $('refresh').addEventListener('click', () => kick(true));
  $('history-refresh').addEventListener('click', () => void history());
  const native = async (verb, args) => { await controller.command(verb, args); if (active) void session.load(); };
  $('play').addEventListener('click', () => void native(controller.selected()?.state?.transport_state === 'playing' ? 'pause' : 'resume'));
  $('next').addEventListener('click', () => void native('next')); $('previous').addEventListener('click', () => void native('previous'));
  $('volume-down').addEventListener('click', () => void native('volume', { delta: -5 })); $('volume-up').addEventListener('click', () => void native('volume', { delta: 5 }));
  for (const b of document.querySelectorAll('[data-command]')) b.addEventListener('click', () => void native(b.dataset.command === 'toggle' ? controller.selected()?.state?.transport_state === 'playing' ? 'pause' : 'resume' : b.dataset.command));
  $('demo-banner').hidden = !demo;
  motion(); render(controller.state); resize();
  if (active) resume(); else suspend();
})();
