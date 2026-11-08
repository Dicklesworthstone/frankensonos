// Spotify sign-in and cache refresh for the Listening Room.
// Network work stays in the shared remote models and on the daemon.
'use strict';

const DashboardMusic = (() => {
  const A = typeof SpotifyConnectRemote !== 'undefined' ? SpotifyConnectRemote : require('./spotify-connect.js');
  const L = typeof LibrarySyncRemote !== 'undefined' ? LibrarySyncRemote : require('./library-sync.js');
  const TABS = Object.freeze(['library', 'connection']);

  function model({ call, changed = () => {}, demo = false, schedule, cancel }) {
    let disposed = false;
    const state = { open: false, active: true, tab: 'library', demo, sync: null, connection: null };
    const routes = { ...A.routes, ...L.routes };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* A view cannot repeat a write. */ } } };
    const request = (operation, args = {}) => {
      const route = routes[operation];
      if (demo || !route) throw new Error('Music setup is unavailable in the preview.');
      return call(route[1], route[0] === 'GET' ? undefined : args.body);
    };
    const sync = L.model({ call: request, changed: publish, schedule, cancel });
    const connection = A.model({ call: request, changed: publish, schedule, cancel });
    state.sync = sync.state; state.connection = connection.state;
    const busy = () => state.sync.busy || state.connection.busy;
    const shown = (tab) => !disposed && state.active && state.open && state.tab === tab && !demo;
    const canStart = () => shown('library') && !busy() && state.sync.fresh
      && !state.sync.loading && Boolean(state.sync.status) && !state.sync.status.running;
    const canConnect = () => shown('connection') && !busy() && state.connection.fresh
      && !state.connection.loading && Boolean(state.connection.status);
    function open(tab = state.tab) {
      if (disposed || !state.active || !TABS.includes(tab)) return Promise.resolve(false);
      state.open = true; state.tab = tab;
      if (tab === 'library') connection.close(); else sync.close();
      publish();
      if (demo) return Promise.resolve(true);
      return (tab === 'library' ? sync : connection).open();
    }
    function close() {
      if (disposed) return;
      state.open = false; sync.close(); connection.close(); publish();
    }
    function refresh() {
      if (!shown(state.tab) || busy()) return Promise.resolve(false);
      return (state.tab === 'library' ? sync : connection).refresh();
    }
    function suspend() {
      if (disposed) return;
      state.active = false; close(); sync.setVisible(false); connection.setVisible(false); publish();
    }
    function resume() {
      if (disposed) return;
      state.active = true; sync.setVisible(true); connection.setVisible(true); publish();
    }
    function dispose() { if (!disposed) { close(); disposed = true; sync.dispose(); connection.dispose(); } }
    return { state, open, close, refresh, canStart, canConnect, busy,
      start: () => canStart() ? sync.start() : Promise.resolve(false),
      reviewConnect: () => canConnect() ? connection.reviewBegin() : null,
      cancelReview: () => connection.cancelReview(),
      begin: (review = state.connection.review) => canConnect() ? connection.begin(review) : Promise.resolve(false),
      complete: (value) => canConnect() ? connection.complete(value) : Promise.resolve(false),
      suspend, resume, dispose };
  }

  function panel({ call, demo = false, browse = () => {}, document: doc = document, schedule, cancel }) {
    const dialog = doc.getElementById('music-dialog');
    if (!dialog) throw new Error('The music setup dialog is missing.');
    let disposed = false, opener = null, observed, review = null, control;
    const el = (tag, text, className) => {
      const node = doc.createElement(tag);
      if (text != null) node.textContent = text;
      if (className) node.className = className;
      return node;
    };
    const button = (id, label, run, className = 'outline-button') => {
      const node = el('button', label, className); node.id = id; node.type = 'button';
      node.addEventListener('click', run); return node;
    };
    const heading = el('h2', 'Your music, ready to play.'); heading.id = 'music-title';
    const closeButton = button('close-music', 'Close', close);
    const header = el('div', null, 'music-header'); header.append(heading, closeButton);
    const intro = el('p', 'Connect Spotify to this daemon, then refresh its library for browsing and DJ sessions. Music setup works even while your rooms are offline.', 'small-note');
    const preview = el('p', 'This is a preview. Open your live daemon to connect Spotify or refresh its cache.', 'music-notice');
    preview.hidden = !demo;
    const tabs = el('div', null, 'music-tabs'); tabs.setAttribute('role', 'tablist'); tabs.setAttribute('aria-label', 'Music setup');
    const tabButtons = new Map(), sections = new Map();
    for (const [name, title] of [['library', 'Library refresh'], ['connection', 'Spotify connection']]) {
      const tab = button('music-tab-' + name, title, () => { void control.open(name); });
      tab.setAttribute('role', 'tab'); tab.setAttribute('aria-controls', 'music-' + name);
      tab.addEventListener('keydown', (event) => {
        const index = TABS.indexOf(name);
        const next = event.key === 'Home' ? 0 : event.key === 'End' ? TABS.length - 1
          : event.key === 'ArrowRight' ? (index + 1) % TABS.length
            : event.key === 'ArrowLeft' ? (index + TABS.length - 1) % TABS.length : null;
        if (next === null) return;
        event.preventDefault(); void control.open(TABS[next]); tabButtons.get(TABS[next]).focus();
      });
      tabButtons.set(name, tab); tabs.append(tab);
      const section = el('section', null, 'music-section'); section.id = 'music-' + name;
      section.setAttribute('role', 'tabpanel'); section.setAttribute('aria-labelledby', tab.id);
      sections.set(name, section);
    }
    const status = (id) => {
      const node = el('p', '', 'music-status'); node.id = id;
      node.setAttribute('role', 'status'); node.setAttribute('aria-live', 'polite'); return node;
    };
    const syncStatus = status('music-sync-status'), syncFresh = el('p', '', 'music-notice');
    syncFresh.id = 'music-sync-fresh';
    const syncRead = button('music-sync-read', 'Check status', () => { void control.refresh(); });
    const syncStart = button('music-sync-start', 'Refresh library from Spotify', () => { void control.start(); }, 'primary-button');
    const browseButton = button('music-browse', 'Browse cached music', () => { close(); browse(); });
    const syncActions = el('div', null, 'music-actions'); syncActions.append(syncRead, syncStart, browseButton);
    const facts = el('div', null, 'music-facts'); facts.id = 'music-sync-facts';
    sections.get('library').append(el('p', 'A refresh reads Spotify into this daemon’s cache. It does not start or skip music; already queued works stay queued.', 'small-note'),
      syncActions, syncStatus, syncFresh, facts);
    const connectStatus = status('music-connection-status'), connectFacts = el('p', '', 'small-note');
    connectFacts.id = 'music-connection-facts';
    const connectRead = button('music-connection-read', 'Check sign-in status', () => { void control.refresh(); });
    const connectBegin = button('music-connection-begin', 'Review new sign-in', () => control.reviewConnect());
    const connectActions = el('div', null, 'music-actions'); connectActions.append(connectRead, connectBegin);
    const reviewBox = el('div', null, 'music-review'); reviewBox.id = 'music-connection-review'; reviewBox.tabIndex = -1;
    const confirm = button('music-connection-confirm', 'Start this sign-in', () => { void control.begin(review); }, 'primary-button');
    const back = button('music-connection-back', 'Cancel', () => { control.cancelReview(); connectBegin.focus(); });
    reviewBox.append(el('p', 'Start a new sign-in for this daemon. This replaces any pending consent link and keeps the cached sign-in. Spotify will ask for read-only library and taste access.'), confirm, back);
    const redirect = el('code', '', 'music-address'); redirect.id = 'music-redirect';
    const consent = el('a', 'Open Spotify consent', 'primary-button'); consent.id = 'music-consent';
    consent.target = '_blank'; consent.rel = 'noopener noreferrer'; consent.referrerPolicy = 'no-referrer';
    const callbackDetails = el('details', null, 'music-callback');
    const callbackLabel = el('label', 'Full callback address from the Spotify tab'); callbackLabel.htmlFor = 'music-callback';
    const callback = el('textarea'); callback.id = 'music-callback'; callback.rows = 3;
    callback.maxLength = A.MAX_ADDRESS; callback.autocomplete = 'off'; callback.autocapitalize = 'off'; callback.spellcheck = false;
    const complete = button('music-connection-complete', 'Finish sign-in on daemon', () => {
      const value = callback.value; callback.value = ''; void control.complete(value);
    });
    callbackDetails.append(el('summary', 'Finish from a phone or another computer'),
      el('p', 'A loopback callback opened on another device can show a connection error. After consent, copy the full address from that Spotify tab and paste it here. It is sent only to this daemon. Keep it private.', 'small-note'),
      callbackLabel, callback, complete);
    const next = button('music-continue', 'Continue to library refresh', () => { void control.open('library'); tabButtons.get('library').focus(); });
    sections.get('connection').append(el('p', 'The daemon stores the sign-in locally. Spotify must also be linked to each household in its Sonos app before those speakers can play Spotify.', 'small-note'),
      connectActions, connectStatus, connectFacts, reviewBox,
      el('p', 'Configured callback — use this exact address when registering the Spotify app:', 'small-note'),
      redirect, consent, callbackDetails,
      el('p', 'After consent, check status here. Then continue to library refresh and explicitly request a refresh.', 'small-note'), next);
    dialog.replaceChildren(header, intro, preview, tabs, sections.get('library'), sections.get('connection'));

    function renderFacts(value) {
      if (observed === value) return;
      observed = value; facts.replaceChildren();
      if (!value) { facts.append(el('p', 'Read status to see the music cached by this daemon.', 'small-note')); return; }
      const deadlines = new Set();
      const card = (title) => {
        const node = el('section', null, 'music-card'); node.append(el('h3', title)); facts.append(node); return node;
      };
      const counts = (node, pairs) => {
        const list = el('dl', null, 'music-counts');
        for (const [label, number] of pairs) list.append(el('dt', label), el('dd', number.toLocaleString()));
        node.append(list);
      };
      const deadline = (node, label, seconds, unique = false) => {
        if (seconds == null || (unique && deadlines.has(seconds))) return;
        if (unique) deadlines.add(seconds);
        const date = new Date(seconds * 1000), time = el('time', date.toLocaleString() + ' (browser local time)');
        time.dateTime = date.toISOString(); const line = el('p', null, 'small-note'); line.append(el('span', label + ' '), time); node.append(line);
      };
      const nextPass = (node, item, label) => {
        const at = [item.retry_at, item.not_before].filter((x) => x !== null);
        if (at.length) deadline(node, label, Math.max(...at));
      };
      if (value.running) facts.append(el('p', 'A library update is running on the daemon. Closing this panel stops observation and lets the update continue.', 'music-notice'));
      if (value.error) facts.append(el('p', 'Last reported failure: ' + value.error, 'music-notice'));
      if (value.library_intake) {
        const item = value.library_intake, node = card('Saved music refresh');
        const phase = { reading: 'Reading saved albums, liked tracks and album tracks.',
          verifying: 'Checking retained pages before they can replace the published library.',
          ready: 'Saved music is retained; the full refresh still needs to finish before publication.' };
        node.append(el('p', item.running ? phase[item.phase]
          : item.pending ? 'The refresh is unfinished. Progress is saved, and the daemon continues in bounded passes.' : 'Saved music intake is complete.'));
        counts(node, [['Validated pages retained', item.pages], ['Tracks retained', item.tracks], ['Retained pages rechecked', item.verified_pages]]);
        if (item.pending) node.append(el('p', 'These are retained progress counts, including repeated tracks across collections. The published library stays unchanged until this read finishes.', 'small-note'));
        deadline(node, 'Spotify cooldown ends at', item.not_before, true);
        if (!item.running && item.pending) nextPass(node, item, 'Next library pass no earlier than');
      }
      const last = card('Last completed full refresh');
      if (!value.last) last.append(el('p', 'No completed full refresh has been reported by this daemon yet.'));
      else {
        const item = value.last; deadline(last, 'Completed at', item.at);
        const pairs = [['Tracks read', item.tracks], ['DJ candidates', item.candidates],
          ['Classical candidates', item.classical], ['Retired tracks', item.retired]];
        if (!value.genre_enrichment) pairs.push(['Artist genres pending', item.genre_artists_pending]);
        counts(last, pairs);
        if (item.notes.length) {
          const notes = el('ul');
          for (const note of item.notes) notes.append(el('li', note));
          last.append(el('strong', 'Not everything was read'), notes);
        }
        if (!item.candidates) last.append(el('p', 'No DJ candidates were reported. Check sign-in, saved music and diagnostics before starting a DJ.', 'small-note'));
      }
      if (value.genre_enrichment) {
        const item = value.genre_enrichment, node = card('Artist genre enrichment');
        node.append(el('p', item.requires_refresh ? 'A full library refresh is needed before genre enrichment can continue.'
          : item.running ? 'A bounded artist genre pass is running.'
            : item.pending ? 'Artist genres are incomplete. The daemon resumes in bounded passes without rereading saved music.' : 'Artist genres are complete for the cached library.'));
        counts(node, [['Artist lookups pending', item.pending], ['Completed in the latest pass', item.completed]]);
        deadline(node, 'Spotify cooldown ends at', item.not_before, true);
        if (!item.running && item.pending && !item.requires_refresh) nextPass(node, item, 'Next genre pass no earlier than');
      }
      if (value.followed_artists) {
        const item = value.followed_artists, node = card('Followed artists');
        const phase = { reading: 'Reading the artists you follow in a bounded pass.',
          verifying: 'Checking retained followed-artist pages before reconciliation.',
          ready: 'Followed artists are retained and verified. A full library refresh still needs to reconcile them.' };
        node.append(el('p', item.requires_refresh ? 'A full library refresh is needed before followed-artist intake can continue.'
          : item.limited ? 'Followed-artist intake reached its cache limit. The retained directory is incomplete; automatic page reads have stopped for this scan.'
            : item.running || item.pending ? phase[item.phase] : 'Followed-artist directory intake is complete.'));
        counts(node, [['Validated pages retained', item.pages], ['Artists retained', item.artists], ['Pages validated for reconciliation', item.verified_pages]]);
        deadline(node, 'Spotify cooldown ends at', item.not_before, true);
        if (!item.running && item.pending && !item.requires_refresh && !item.limited) {
          nextPass(node, item, item.phase === 'ready' ? 'Next library reconciliation no earlier than' : 'Next followed-artist pass no earlier than');
        }
      }
    }

    function render(state) {
      if (disposed || !control) return;
      const s = state.sync, a = state.connection, busy = s.busy || a.busy, visible = state.open && state.active;
      if (!visible && dialog.open) dialog.close();
      for (const node of doc.querySelectorAll('[data-open-music]')) node.setAttribute('aria-expanded', String(visible));
      for (const name of TABS) {
        const selected = state.tab === name;
        tabButtons.get(name).setAttribute('aria-selected', String(selected));
        tabButtons.get(name).tabIndex = selected ? 0 : -1;
        sections.get(name).hidden = !selected;
      }
      sections.get('library').setAttribute('aria-busy', String(s.loading || s.busy));
      syncRead.disabled = demo || !visible || s.loading || busy;
      syncStart.disabled = !control.canStart();
      browseButton.disabled = !visible;
      syncStatus.textContent = demo ? '' : s.busy ? 'Requesting a library refresh…'
        : s.loading ? 'Reading library refresh status…' : s.message || s.status?.done || '';
      syncFresh.hidden = demo || s.fresh || !s.status;
      syncFresh.textContent = 'Last observation shown. Read fresh status before requesting another refresh.';
      renderFacts(demo ? null : s.status);
      facts.setAttribute('aria-label', s.fresh ? 'Observed library refresh' : 'Last observed library refresh; may be stale');
      sections.get('connection').setAttribute('aria-busy', String(a.loading || a.busy));
      connectRead.disabled = demo || !visible || a.loading || busy;
      connectBegin.disabled = !control.canConnect();
      confirm.disabled = !control.canConnect() || !a.review;
      back.disabled = busy;
      // A periodic read must not erase a callback while the owner is pasting it.
      callback.disabled = demo || !visible || state.tab !== 'connection' || !a.status?.pending || busy;
      if (!visible || state.tab !== 'connection' || busy) callback.value = '';
      complete.disabled = !control.canConnect() || !a.status?.pending;
      next.disabled = !control.canConnect() || !a.status?.signed_in;
      connectStatus.textContent = demo ? '' : a.loading ? 'Reading sign-in status…' : a.message;
      connectFacts.textContent = !a.status ? 'Sign-in status has not been read.'
        : (!a.fresh ? 'Last observation (may be stale): ' : '') + (a.status.signed_in
          ? 'A cached sign-in is available; refresh the library to verify Spotify access.' : 'No cached sign-in is available.')
          + (a.status.pending ? ' A sign-in is waiting or being completed.' : '');
      redirect.textContent = a.status?.redirect_uri || 'Check status to see the configured callback.';
      consent.hidden = demo || !visible || state.tab !== 'connection' || !a.consent || !a.fresh || busy;
      if (consent.hidden) consent.removeAttribute('href'); else consent.setAttribute('href', a.consent.url);
      reviewBox.hidden = !a.review;
      if (review !== a.review) { review = a.review; if (review && visible) reviewBox.focus({ preventScroll: true }); }
    }
    control = model({ call, changed: render, demo, schedule, cancel });
    function close() {
      callback.value = ''; control.close();
      if (dialog.open) dialog.close();
      if (opener?.isConnected) opener.focus(); opener = null;
    }
    function open(tab = 'library', source = null) {
      if (disposed || doc.hidden || !control.state.active || !TABS.includes(tab)) return false;
      opener = source || doc.activeElement;
      void control.open(tab);
      if (!dialog.open) dialog.showModal();
      tabButtons.get(tab).focus(); return true;
    }
    const activate = (event) => {
      const source = event.target?.closest?.('[data-open-music]');
      if (!source || !TABS.includes(source.dataset.openMusic)) return;
      event.preventDefault();
      const search = doc.getElementById('search-dialog'); if (search?.open) search.close();
      open(source.dataset.openMusic, source);
    };
    const cancelled = (event) => { event.preventDefault(); close(); };
    const closed = () => { if (control.state.open) close(); };
    doc.addEventListener('click', activate); dialog.addEventListener('cancel', cancelled); dialog.addEventListener('close', closed);
    if (doc.hidden) control.suspend(); render(control.state);
    return { control, open,
      suspend() { callback.value = ''; control.suspend(); if (dialog.open) dialog.close(); opener = null; },
      resume() { control.resume(); },
      dispose() {
        if (disposed) return;
        callback.value = ''; control.dispose(); disposed = true; if (dialog.open) dialog.close();
        doc.removeEventListener('click', activate); dialog.removeEventListener('cancel', cancelled); dialog.removeEventListener('close', closed);
      } };
  }
  return { model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = DashboardMusic;
