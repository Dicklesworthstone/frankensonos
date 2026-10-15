// Listening Room: a same-origin dashboard of real state, with a read-only design preview.
'use strict';
(() => {
  const C = DashboardCore, $ = (id) => document.getElementById(id);
  const demo = new URLSearchParams(location.search).get('demo') === '1' || document.body.dataset.preview === 'true';
  const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
  function icon(name) {
    const n = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    const u = document.createElementNS(n.namespaceURI, 'use'); u.setAttribute('href', `#i-${name}`); n.append(u); n.setAttribute('aria-hidden', 'true'); return n;
  }
  const demoZones = [
    { household: 'S2', coordinator_room: 'Studio', members: ['Studio', 'Kitchen'], transport_state: 'playing' },
    { household: 'S2', coordinator_room: 'Terrace', members: ['Terrace'], transport_state: 'paused' },
    { household: 'S1', coordinator_room: 'Reading room', members: ['Reading room'], transport_state: 'stopped' },
  ];
  const demoTracks = [
    { title: 'Night Bloom', creator: 'The Quiet Hours', album: 'Soft Focus', uri: 'demo:night-bloom', duration_secs: 286, position_secs: 94, queue_position: 2 },
    { title: 'A Place Between', creator: 'Low Tide', album: 'Stillwater', uri: 'demo:stillwater', duration_secs: 241, position_secs: 68, queue_position: 1 },
  ];
  const request = C.request();
  async function call(url, body) {
    if (!demo) return request(url, body);
    if (body !== undefined) throw new Error('Playback is disabled in design preview.');
    if (url === '/zones') return structuredClone(demoZones);
    if (url.endsWith('/state')) {
      const zone = demoZones.find((z) => url.includes(encodeURIComponent(C.key(z))));
      const i = demoZones.indexOf(zone);
      return { zone, transport_state: zone.transport_state, volume: i === 0 ? 28 : 18, group_volume: 28,
        mute: false, track: demoTracks[i] || null };
    }
    if (url.endsWith('/dj')) return { running: true, steering: { mood: 'late-night' },
      now: { reason: { summary: 'A quieter turn, with a little room to wander.' } }, next: [{ title: 'Weightless', composer: 'Field Notes' }] };
    if (url.startsWith('/history')) return ['Night Bloom', 'A Place Between', 'Weightless', 'Golden Hour'].map((title, i) => ({
      title, artist: ['The Quiet Hours', 'Low Tide', 'Field Notes', 'Sunday Service'][i], source_uri: `demo:${i}`, played_at: Math.floor(Date.now() / 1000) - (i + 1) * 320,
    }));
    return null;
  }
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let motionPaused = reduced.matches, immersive = false, artKey = '', detailsVersion = 0, lastDetails = -Infinity;
  let live = null, timer = null, timerDue = 0, active = true, pending = false, explicitPending = false, inFlight = false, backoff = 15000, retryAt = 0;
  let lastRooms = '', lastMap = '', lastTrack = '', djTarget = '', previousFocus = null;
  const renderer = DashboardVisualizer.create($('visualizer'), { reduced: reduced.matches });
  const audio = DashboardAudio.create({ changed(state) {
    const running = state.status === 'live', pending = state.status === 'requesting';
    $('microphone').textContent = running ? 'Stop microphone' : pending ? 'Cancel microphone' : 'Use microphone';
    $('microphone').setAttribute('aria-pressed', String(running));
    $('audio-status').textContent = state.message;
    $('visual-source').textContent = running ? 'LIVE MICROPHONE · LOCAL ONLY' : 'AMBIENT · NOT AUDIO ANALYSIS';
    renderer.audio(running ? audio.sample : null);
  } });
  $('microphone').addEventListener('click', () => {
    if (['requesting', 'live'].includes(audio.state.status)) audio.stop();
    else void audio.start();
  });
  const controller = C.model({ call, demo, changed: render });
  const chartResize = () => { const r = $('stage').getBoundingClientRect(); renderer.resize(r.width, r.height, window.devicePixelRatio || 1); };
  const observer = new ResizeObserver(chartResize); observer.observe($('stage'));
  function motionLabel() {
    $('motion').textContent = !renderer.supported ? 'Canvas unavailable' : motionPaused ? 'Resume motion' : 'Pause motion';
    $('motion').setAttribute('aria-pressed', String(motionPaused)); $('motion').disabled = !renderer.supported;
  }
  $('motion').addEventListener('click', () => { motionPaused = !motionPaused; renderer.pause(motionPaused); motionLabel(); });
  const onMotion = () => { motionPaused = reduced.matches; renderer.pause(motionPaused); motionLabel(); };
  reduced.addEventListener('change', onMotion);
  for (const b of document.querySelectorAll('[data-mode]')) b.addEventListener('click', () => {
    renderer.mode(b.dataset.mode);
    for (const other of document.querySelectorAll('[data-mode]')) other.setAttribute('aria-pressed', String(other === b));
  });
  function immerse(value) {
    if (value === immersive) return;
    immersive = value;
    if (value) previousFocus = document.activeElement;
    $('stage').classList.toggle('immersive', value); document.body.classList.toggle('is-immersive', value);
    $('exit-immerse').hidden = !value; $('immerse').hidden = value;
    // Inert the other content so keyboard and screen-reader focus stays in the stage.
    for (const n of document.querySelectorAll('.sidebar,.topbar,.summary,.notice,.rooms-panel,.detail-grid,.history-section,.page-footer,.outcome')) n.inert = value;
    if (value) {
      $('exit-immerse').focus();
      $('stage').requestFullscreen?.().catch(() => { /* CSS immersion remains usable. */ });
    } else {
      if (document.fullscreenElement) document.exitFullscreen?.().catch(() => {});
      previousFocus?.focus();
    }
    chartResize();
  }
  $('immerse').addEventListener('click', () => immerse(true));
  $('nav-immerse').addEventListener('click', () => immerse(true));
  $('exit-immerse').addEventListener('click', () => immerse(false));
  document.addEventListener('fullscreenchange', () => { if (!document.fullscreenElement && immersive) immerse(false); });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && immersive) { e.preventDefault(); immerse(false); }
    if (immersive && e.key === 'Tab') {
      const controls = [...$('stage').querySelectorAll('button:not(:disabled)')].filter((b) => !b.hidden && b.getClientRects().length);
      const first = controls[0], last = controls.at(-1);
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    }
  });
  function render(s) {
    $('connection').textContent = demo ? 'Design preview' : !s.active ? 'Paused while hidden' : s.loading ? 'Refreshing…' : s.error ? 'Check connection' : s.connected ? 'Live on your network' : s.initialized ? 'Snapshot · reconnecting' : 'Connecting…';
    $('connection').dataset.connected = String(!demo && s.connected && !s.error);
    $('error').hidden = !s.error; $('error').textContent = s.error;
    $('outcome').textContent = s.message;
    $('stage-outcome').textContent = s.message;
    $('refresh').disabled = s.loading || s.busy;
    const rooms = s.items.reduce((n, i) => n + i.zone.members.length, 0);
    const playing = s.items.filter((i) => i.fresh && i.state?.transport_state === 'playing').length;
    const homes = new Set(s.items.map((i) => i.zone.household)).size;
    $('nav-count').textContent = rooms || '—';
    const stats = [[rooms, 'rooms discovered'], [playing, 'groups playing'], [homes, 'households']];
    $('summary').replaceChildren(...stats.map(([count, label]) => { const n = el('span'); n.append(el('strong', '', s.initialized ? count : '—'), ` ${label}`); return n; }));
    const item = controller.selected(), track = item?.state?.track;
    const fresh = item?.fresh && s.active;
    const title = track?.title || track?.uri || (s.initialized ? 'Ready when you are.' : 'Make room for music.');
    $('track-title').textContent = title; $('track-title').title = title;
    $('track-by').textContent = track ? [track.creator, track.album].filter((x) => typeof x === 'string').join(' · ') : 'Choose something in the full remote to begin.';
    $('target').textContent = item ? `${item.zone.members.join(' + ')} / ${item.zone.household}` : s.items.length ? 'SELECT A ROOM' : 'YOUR HOUSE IS QUIET';
    $('stage-status').textContent = demo ? 'PREVIEW SESSION' : !fresh ? 'AWAITING FRESH STATE' : item.state.transport_state.toUpperCase();
    $('source-label').textContent = demo ? 'DESIGN PREVIEW' : !fresh ? 'LAST OBSERVATION' : track?.uri?.includes('spotify') ? 'SPOTIFY ON SONOS' : track ? 'SONOS PLAYBACK' : 'NOTHING LOADED';
    const isPlaying = item?.state?.transport_state === 'playing';
    $('play-icon').firstElementChild.setAttribute('href', isPlaying ? '#i-pause' : '#i-play');
    $('play').setAttribute('aria-label', `${isPlaying ? 'Pause' : 'Play'} ${item?.zone.members.join(' + ') || 'selected group'}`);
    const allowed = controller.canControl();
    for (const id of ['play', 'next', 'previous']) $(id).disabled = !allowed || !track || !['playing', 'paused', 'stopped'].includes(item?.state?.transport_state);
    const volume = item?.zone.members.length === 1 ? item?.state?.volume : item?.state?.group_volume;
    $('group-volume').textContent = Number.isInteger(volume) ? volume : '—';
    $('group-volume').title = 'Selected group volume';
    $('volume-down').disabled = $('volume-up').disabled = !allowed || !Number.isInteger(volume);
    const identity = `${s.selected}|${track?.uri}|${track?.title}`;
    const visualIdentity = `${identity}|${!!fresh && isPlaying}`;
    if (visualIdentity !== lastTrack) { lastTrack = visualIdentity; renderer.track(C.hash(identity), !!fresh && isPlaying); }
    const art = demo || !track ? '' : `/art?${new URLSearchParams({ zone: s.selected, v: C.hash(identity).toString(36) })}`;
    if (art !== artKey) { artKey = art; $('art').hidden = true; if (art) $('art').src = art; else $('art').removeAttribute('src'); }
    renderRooms(s); renderMap(s); timing();
    if (s.selected !== djTarget) { djTarget = s.selected; void details(true); }
  }
  $('art').addEventListener('load', () => { if ($('art').getAttribute('src') === artKey) $('art').hidden = false; });
  $('art').addEventListener('error', () => { $('art').hidden = true; });
  function renderRooms(s) {
    const signature = JSON.stringify([s.selected, s.busy, s.items.map((i) => [i.zone, i.fresh, i.state?.track?.title, i.state?.transport_state])]);
    if (signature === lastRooms) return; lastRooms = signature;
    const focus = document.activeElement?.dataset?.room;
    $('rooms').replaceChildren();
    if (!s.items.length) $('rooms').append(el('p', 'empty', s.initialized
      ? 'No speakers discovered. Check that the daemon is on the speaker LAN, then open the full remote for diagnosis.' : 'Looking for your speakers…'));
    for (const i of s.items) {
      const b = el('button', 'room-card'); b.type = 'button'; b.dataset.room = C.key(i.zone); b.disabled = s.busy;
      b.setAttribute('aria-pressed', String(C.key(i.zone) === s.selected));
      b.setAttribute('aria-label', `Select ${i.zone.members.join(' + ')} in ${i.zone.household}`);
      const head = el('div', 'room-head'), symbol = el('span', 'room-icon'); symbol.append(icon('speaker'));
      const name = el('div'); name.append(el('div', 'room-name', i.zone.coordinator_room), el('div', 'room-house', i.zone.household));
      head.append(symbol, name, el('span', `room-state${!i.fresh ? ' stale' : ''}`, !i.fresh ? 'stale' : i.state.transport_state));
      b.append(head, el('p', 'room-track', i.state?.track?.title || (i.state?.track ? 'Untitled source' : 'Nothing playing')),
        el('p', 'room-members', i.zone.members.length > 1 ? `With ${i.zone.members.filter((r) => r !== i.zone.coordinator_room).join(', ')}` : i.zone.degraded.length ? 'Bonded player missing' : 'Independent room'));
      b.addEventListener('click', () => controller.select(C.key(i.zone))); $('rooms').append(b);
    }
    if (focus) [...$('rooms').querySelectorAll('button')].find((n) => n.dataset.room === focus)?.focus({ preventScroll: true });
  }
  function renderMap(s) {
    const signature = JSON.stringify(s.items.map((i) => [i.zone, i.fresh]));
    if (signature === lastMap) return; lastMap = signature;
    $('topology').replaceChildren();
    for (const i of s.items) {
      const row = el('div', 'topology-row'), group = el('div', 'topology-group');
      for (const room of i.zone.members) group.append(el('span', `topology-node${room === i.zone.coordinator_room ? ' leader' : ''}${!i.fresh ? ' stale' : ''}`, room));
      row.append(el('span', 'home-node', i.zone.household), group); $('topology').append(row);
    }
    $('topology').append(el('p', 'map-note', s.items.length ? 'Highlighted: group coordinator. Linked rooms share playback. Dashed: stale observation.' : 'No groups discovered yet. The map never invents missing rooms.'));
  }
  function timing() {
    const item = controller.selected(), at = C.elapsed(item, performance.now()), duration = item?.state?.track?.duration_secs;
    $('elapsed').textContent = `${item?.fresh && item?.state?.transport_state === 'playing' && at !== null ? '≈ ' : ''}${C.clock(at)}`;
    $('duration').textContent = C.clock(duration);
    $('progress').max = duration > 0 ? duration : 1; $('progress').value = at !== null && duration > 0 ? at : 0;
    $('progress').setAttribute('aria-label', at === null ? 'Track position unavailable' : `Estimated position ${C.clock(at)} of ${C.clock(duration)}`);
  }
  async function details(force = false) {
    if (!active || (!force && performance.now() - lastDetails < 30000)) return;
    lastDetails = performance.now(); const version = ++detailsVersion, target = controller.state.selected;
    $('dj').replaceChildren(el('p', 'empty', target ? 'Reading this room’s DJ…' : 'Select a room for DJ insight.'));
    if (!target) return;
    try {
      const result = await call(`/zones/${encodeURIComponent(target)}/dj`);
      if (version !== detailsVersion || !active) return;
      if (!result || typeof result.running !== 'boolean') throw new Error('DJ status is unavailable.');
      $('dj').replaceChildren(el('span', 'dj-mood', result.running ? result.steering?.mood || 'Your personal DJ' : 'DJ is resting'),
        el('p', 'dj-copy', result.running ? result.now?.reason?.summary || 'Making room for your next favorite.' : 'A soundtrack that knows you.'),
        el('p', 'dj-sub', result.problem || (result.next?.[0]?.title ? `Up next · ${result.next[0].title}` : result.running ? 'Picks follow your library and preferences.' : 'Start a DJ session from the full remote.')));
    } catch (e) { if (version === detailsVersion && active) $('dj').replaceChildren(el('p', 'empty', C.problem(e))); }
  }
  let historyVersion = 0, historyBusy = false;
  async function history() {
    if (!active || historyBusy) return;
    historyBusy = true; $('history-refresh').disabled = true; const version = ++historyVersion;
    try {
      const rows = await call('/history?limit=4');
      if (version !== historyVersion || !active) return;
      if (!Array.isArray(rows) || rows.length > 4 || rows.some((r) => !r || typeof r.source_uri !== 'string' || !Number.isSafeInteger(r.played_at)
        || Math.abs(r.played_at) > 8640000000000)) throw new Error('Listening history is unreadable.');
      $('history').replaceChildren();
      for (const r of rows) {
        const row = el('article', 'history-item'), art = el('div', 'history-art'), copy = el('div', 'history-copy');
        art.dataset.color = C.hash(r.source_uri) % 4; art.setAttribute('aria-hidden', 'true');
        const stamp = new Date(r.played_at * 1000), time = el('time', '', stamp.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }));
        time.dateTime = stamp.toISOString(); time.title = stamp.toLocaleString();
        copy.append(el('strong', '', r.title || r.source_uri), el('p', '', r.artist || 'Sonos playback'), time); row.append(art, copy); $('history').append(row);
      }
      if (!rows.length) $('history').append(el('p', 'empty', 'No recorded plays yet. Your soundtrack starts here.'));
    } catch (e) { if (version === historyVersion && active) $('history').replaceChildren(el('p', 'empty', C.problem(e))); }
    finally { historyBusy = false; $('history-refresh').disabled = false; }
  }
  function clearTimer() { if (timer !== null) clearTimeout(timer); timer = null; }
  function schedule(delay = 350, explicit = false) {
    if (!active) return;
    pending = true; explicitPending ||= explicit;
    if (inFlight || !Number.isFinite(retryAt)) return;
    const wait = Math.max(delay, retryAt - Date.now());
    const due = Date.now() + Math.min(60000, Math.max(0, wait));
    if (timer !== null && timerDue <= due) return;
    clearTimer(); timerDue = due;
    timer = setTimeout(async () => {
      timer = null; if (!active) return;
      if (Date.now() < retryAt) { schedule(); return; }
      inFlight = true; pending = false;
      const explicitRead = explicitPending; explicitPending = false;
      const ok = await controller.refresh(explicitRead);
      inFlight = false;
      if (!active) return;
      retryAt = Date.now() + Math.max(0, controller.state.retryAfter);
      backoff = ok ? 15000 : Math.min(60000, backoff * 2);
      if (ok) void details();
      // Periodic read is a bounded safety net for missed events or a silent feed.
      schedule(pending ? 500 : backoff);
    }, Math.min(60000, Math.max(0, wait)));
  }
  function connect() {
    if (demo || !active || live || typeof EventSource === 'undefined') return;
    live = new EventSource('/events');
    const source = live;
    const current = () => active && live === source;
    source.onopen = () => { if (current()) { controller.connection(true); schedule(); } };
    source.onerror = () => { if (current()) controller.connection(false); };
    for (const event of C.EVENTS) source.addEventListener(event, () => { if (current()) schedule(); });
  }
  function suspend() {
    audio.stop('Microphone stopped while the page is hidden. Enable it again explicitly to resume.');
    active = false; pending = explicitPending = false; clearTimer(); if (live) live.close(); live = null;
    ++detailsVersion; ++historyVersion; controller.suspend(); renderer.visible(false);
  }
  function resume() {
    if (document.hidden) return;
    active = true; controller.resume(); renderer.visible(true); connect(); schedule(0); void history(); void details(true);
  }
  document.addEventListener('visibilitychange', () => document.hidden ? suspend() : resume());
  window.addEventListener('pagehide', suspend); window.addEventListener('pageshow', () => { if (!active) resume(); });
  window.addEventListener('online', () => { connect(); schedule(); });
  $('refresh').addEventListener('click', () => { schedule(0, true); void details(true); void history(); });
  $('history-refresh').addEventListener('click', () => void history());
  $('play').addEventListener('click', () => void controller.command(controller.selected()?.state?.transport_state === 'playing' ? 'pause' : 'resume'));
  $('next').addEventListener('click', () => void controller.command('next'));
  $('previous').addEventListener('click', () => void controller.command('previous'));
  $('volume-down').addEventListener('click', () => void controller.command('volume', { delta: -5 }));
  $('volume-up').addEventListener('click', () => void controller.command('volume', { delta: 5 }));
  setInterval(() => { if (active) { timing(); if (!controller.canControl()) for (const id of ['play', 'next', 'previous', 'volume-down', 'volume-up']) $(id).disabled = true; } }, 1000);
  $('demo-banner').hidden = !demo;
  motionLabel(); render(controller.state); chartResize();
  if (!document.hidden) resume(); else suspend();
})();
