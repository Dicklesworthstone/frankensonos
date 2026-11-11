// Refresh the daemon's music cache; every network read stays on the daemon.
'use strict';

const LibrarySyncRemote = (() => {
  const routes = {
    librarySyncStatus: ['GET', '/dj/sync'],
    librarySyncStart: ['POST', '/dj/sync'],
  };
  const POLL_MS = 3000;
  const PENDING_POLL_MS = 60000;
  const MAX_BACKOFF_MS = 60000;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const count = (v) => Number.isSafeInteger(v) && v >= 0;
  const timestamp = (v) => Number.isSafeInteger(v) && Math.abs(v) <= 8640000000000;
  const text = (v) => typeof v === 'string';
  const message = (error) => error && text(error.message)
    ? `${error.message}${error.hint ? ` ${error.hint}` : ''}` : 'The library request failed.';

  function statusOf(value) {
    if (!object(value) || !text(value.done) || typeof value.running !== 'boolean'
        || (value.error != null && !text(value.error))) {
      throw new Error('The library refresh status is unreadable. Check status again.');
    }
    let last = null;
    if (value.last != null) {
      const v = value.last;
      if (!object(v) || !timestamp(v.at)
          || !['tracks', 'candidates', 'classical', 'retired'].every((key) => count(v[key]))
          || !count(v.genre_artists_pending ?? 0) || v.candidates > v.tracks || v.classical > v.candidates
          || !Array.isArray(v.notes ?? []) || !(v.notes ?? []).every(text)) {
        throw new Error('The last library refresh is incomplete. Check status again.');
      }
      last = Object.freeze({ at: v.at, tracks: v.tracks, candidates: v.candidates,
        classical: v.classical, retired: v.retired, genre_artists_pending: v.genre_artists_pending ?? 0,
        notes: Object.freeze([...(v.notes ?? [])]) });
    }
    let genres = null;
    if (value.genre_enrichment != null) {
      const v = value.genre_enrichment;
      if (!object(v) || !count(v.pending) || !count(v.completed)
          || typeof v.running !== 'boolean' || typeof v.requires_refresh !== 'boolean'
          || (v.running && !value.running)
          || (v.retry_at != null && !timestamp(v.retry_at))
          || (v.not_before != null && !timestamp(v.not_before))) {
        throw new Error('The artist genre enrichment status is unreadable. Check status again.');
      }
      genres = Object.freeze({ pending: v.pending, completed: v.completed, running: v.running,
        retry_at: v.retry_at ?? null, not_before: v.not_before ?? null, requires_refresh: v.requires_refresh });
    }
    let intake = null;
    if (value.library_intake != null) {
      const v = value.library_intake;
      if (!object(v) || typeof v.pending !== 'boolean' || typeof v.running !== 'boolean'
          || !['reading', 'verifying', 'ready'].includes(v.phase)
          || !['pages', 'tracks', 'verified_pages'].every((key) => count(v[key]))
          || v.verified_pages > v.pages || (v.running && (!value.running || genres?.running))
          || (v.retry_at != null && !timestamp(v.retry_at))
          || (v.not_before != null && !timestamp(v.not_before))) {
        throw new Error('The saved library progress is unreadable. Check status again.');
      }
      intake = Object.freeze({ pending: v.pending, phase: v.phase, pages: v.pages, tracks: v.tracks,
        verified_pages: v.verified_pages, running: v.running,
        retry_at: v.retry_at ?? null, not_before: v.not_before ?? null });
    }
    let followed = null;
    if (value.followed_artists != null) {
      const v = value.followed_artists;
      if (!object(v) || !['pending', 'limited', 'running', 'requires_refresh'].every((key) => typeof v[key] === 'boolean')
          || !['reading', 'verifying', 'ready'].includes(v.phase)
          || !['pages', 'artists', 'verified_pages'].every((key) => count(v[key]))
          || v.verified_pages > v.pages || (v.limited && v.pending)
          || (v.running && (!value.running || genres?.running || intake?.running))
          || (v.retry_at != null && !timestamp(v.retry_at))
          || (v.not_before != null && !timestamp(v.not_before))) {
        throw new Error('The followed-artist progress is unreadable. Check status again.');
      }
      followed = Object.freeze({ pending: v.pending, phase: v.phase, pages: v.pages, artists: v.artists,
        verified_pages: v.verified_pages, limited: v.limited, running: v.running,
        retry_at: v.retry_at ?? null, not_before: v.not_before ?? null, requires_refresh: v.requires_refresh });
    }
    let samples = null;
    if (value.artist_sampling != null) {
      const v = value.artist_sampling;
      if (!object(v) || !['pending', 'completed', 'attempted'].every((key) => count(v[key]))
          || !['limited', 'running', 'requires_refresh'].every((key) => typeof v[key] === 'boolean')
          || (v.running && (!value.running || genres?.running || intake?.running || followed?.running))
          || (v.retry_at != null && !timestamp(v.retry_at))
          || (v.not_before != null && !timestamp(v.not_before))) {
        throw new Error('The artist sampling progress is unreadable. Check status again.');
      }
      samples = Object.freeze({ pending: v.pending, completed: v.completed, attempted: v.attempted,
        limited: v.limited, running: v.running, retry_at: v.retry_at ?? null,
        not_before: v.not_before ?? null, requires_refresh: v.requires_refresh });
    }
    let recent = null;
    if (value.recent_listening != null) {
      const v = value.recent_listening;
      if (!object(v) || !['running', 'more_available', 'missing_scope', 'requires_refresh'].every((key) => typeof v[key] === 'boolean')
          || !['received', 'accepted', 'skipped'].every((key) => count(v[key]) && v[key] <= 50)
          || v.accepted + v.skipped > v.received || (v.missing_scope && v.requires_refresh)
          || (v.running && (!value.running || genres?.running || intake?.running || followed?.running || samples?.running))
          || (v.checked_at != null && (!timestamp(v.checked_at) || v.checked_at < 0))
          || (v.checked_at == null && v.accepted > 0)
          || (v.retry_at != null && !timestamp(v.retry_at))
          || (v.not_before != null && !timestamp(v.not_before))
          || (v.error != null && !text(v.error))) {
        throw new Error('The recent listening status is unreadable. Check status again.');
      }
      recent = Object.freeze({ running: v.running, checked_at: v.checked_at ?? null,
        received: v.received, accepted: v.accepted, skipped: v.skipped, more_available: v.more_available,
        missing_scope: v.missing_scope, requires_refresh: v.requires_refresh,
        retry_at: v.retry_at ?? null, not_before: v.not_before ?? null, error: v.error ?? null });
    }
    return Object.freeze({ done: value.done, running: value.running, last,
      error: value.error ?? null, genre_enrichment: genres, library_intake: intake, followed_artists: followed,
      recent_listening: recent, artist_sampling: samples });
  }

  function model({ call, changed = () => {}, schedule = setTimeout, cancel = clearTimeout }) {
    let epoch = 0, disposed = false, visible = true, timer = null;
    let readFlight = false, readAgain = false, failures = 0, startProblem = '';
    const state = { open: false, loading: false, busy: false, fresh: false,
      status: null, message: '', uncertain: false };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* View failure is not a write failure. */ } } };
    const active = () => !disposed && state.open && visible;
    const current = (version) => active() && version === epoch;
    const stopTimer = () => { if (timer !== null) cancel(timer); timer = null; };

    function arm() {
      stopTimer();
      const genres = state.status?.genre_enrichment;
      const followed = state.status?.followed_artists;
      const samples = state.status?.artist_sampling;
      const pending = state.status?.library_intake?.pending || (genres && (genres.pending > 0 || genres.requires_refresh))
        || (followed && (followed.pending || followed.requires_refresh))
        || (samples && ((samples.pending > 0 && !samples.limited) || samples.requires_refresh));
      if (!active() || state.busy || readFlight || (!failures && !state.status?.running && !pending)) return;
      const delay = failures ? Math.min(MAX_BACKOFF_MS, POLL_MS * 2 ** Math.min(failures, 5))
        : state.status?.running ? POLL_MS : PENDING_POLL_MS;
      timer = schedule(() => { timer = null; void refresh(); }, delay);
    }

    async function refresh() {
      if (!active()) return false;
      if (state.busy || readFlight) { readAgain = true; return false; }
      stopTimer(); readAgain = false; readFlight = true;
      const version = ++epoch;
      state.loading = true; state.fresh = false; publish();
      try {
        const answer = statusOf(await call('librarySyncStatus'));
        if (!current(version)) return false;
        state.status = answer; state.fresh = true; failures = 0;
        state.message = state.uncertain
          ? `${startProblem} The refresh request had no confirmed reply. This is the latest daemon status; nothing was retried.` : '';
        return true;
      } catch (error) {
        if (current(version)) { ++failures; state.message = message(error); }
        return false;
      } finally {
        readFlight = false;
        if (!disposed) {
          state.loading = false; publish();
          if (active() && readAgain && !state.busy) { readAgain = false; void refresh(); }
          else arm();
        }
      }
    }

    async function start() {
      if (!active() || state.busy || readFlight || !state.fresh || !state.status || state.status.running) return false;
      stopTimer(); readAgain = false; const version = ++epoch;
      state.busy = true; state.fresh = false; state.uncertain = false; startProblem = '';
      state.message = 'Requesting a library refresh…'; publish();
      let acknowledged = false;
      try {
        if (!current(version)) return false;
        const answer = statusOf(await call('librarySyncStart', { body: {} }));
        acknowledged = true;
        if (current(version)) {
          state.status = answer; state.fresh = true; failures = 0; state.message = '';
        } else readAgain = true;
        return true;
      } catch (error) {
        if (!disposed) {
          state.uncertain = true; ++failures; startProblem = message(error);
          state.message = `${message(error)} The request may have reached the daemon. Check status; do not repeat it blindly.`;
        }
        return false;
      } finally {
        state.busy = false;
        if (!disposed) {
          publish();
          // Reads recover lost replies, including when a panel reopened during a write.
          // No path here dispatches another refresh request.
          if (active() && (readAgain || !acknowledged)) { readAgain = false; void refresh(); }
          else arm();
        }
      }
    }

    function open() { if (disposed) return Promise.resolve(false); state.open = true; publish(); return refresh(); }
    function close() {
      if (disposed) return;
      ++epoch; stopTimer(); readAgain = false; state.open = false; state.fresh = false; publish();
    }
    function setVisible(value) {
      if (disposed || visible === Boolean(value)) return;
      visible = Boolean(value); ++epoch; stopTimer(); state.fresh = false;
      if (active()) void refresh();
      else { readAgain = false; publish(); }
    }
    function dispose() { close(); disposed = true; }
    return { state, open, close, refresh, start, setVisible, dispose };
  }

  let nextId = 0;
  function panel({ call, el }) {
    const id = `library-sync-${++nextId}`;
    const button = (text, onclick) => el('button', { type: 'button', text, onclick });
    const toggle = button('Spotify library', () => { if (control.state.open) control.close(); else void control.open(); });
    toggle.setAttribute('aria-controls', id);
    const box = el('div', { id, class: 'library-sync-panel', hidden: true, tabindex: -1 });
    const check = button('Check refresh status', () => { void control.refresh(); });
    const start = button('Refresh library from Spotify', () => { void control.start(); });
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const facts = el('div', { class: 'library-sync-facts' });
    let observed = null;
    const control = model({ call, changed: render });

    function render(state) {
      box.hidden = !state.open; toggle.setAttribute('aria-expanded', String(state.open));
      box.setAttribute('aria-busy', String(state.loading || state.busy));
      check.disabled = state.loading || state.busy;
      start.disabled = !state.fresh || state.loading || state.busy || !state.status || state.status.running;
      status.textContent = state.busy ? 'Requesting a library refresh…'
        : state.loading ? 'Reading library refresh status…'
          : state.message || state.status?.done || '';
      if (observed !== state.status) {
        observed = state.status; facts.replaceChildren();
        if (observed) {
          const deadline = (label, seconds) => {
            if (seconds === null) return;
            const at = new Date(seconds * 1000);
            facts.append(el('p', {}, el('span', { text: `${label} ` }),
              el('time', { datetime: at.toISOString(), text: `${at.toLocaleString()} (browser local time)` })));
          };
          if (observed.running) facts.append(el('p', { text: 'A library update is running on the daemon. Status is checked while this panel is visible; closing it does not stop the update.' }));
          if (observed.error) facts.append(el('p', { class: 'library-sync-warning', text: `Last reported failure: ${observed.error}` }));
          if (observed.library_intake) {
            const intake = observed.library_intake;
            const phase = { reading: 'Reading saved albums, liked tracks and album tracks.',
              verifying: 'Checking retained pages before they can replace the published library.',
              ready: 'Saved music is retained; the full refresh still needs to finish before publication.' };
            facts.append(el('h3', { text: 'Saved music refresh' }),
              el('p', { text: intake.running ? phase[intake.phase]
                : intake.pending ? 'The saved music refresh is unfinished. Progress is saved, and the daemon will continue in bounded passes.'
                  : 'Saved music intake is complete.' }));
            facts.append(el('dl', { class: 'library-sync-counts' },
              el('dt', { text: 'Validated pages retained' }), el('dd', { text: intake.pages.toLocaleString() }),
              el('dt', { text: 'Tracks retained' }), el('dd', { text: intake.tracks.toLocaleString() }),
              el('dt', { text: 'Retained pages rechecked' }), el('dd', { text: intake.verified_pages.toLocaleString() })));
            if (intake.pending) facts.append(el('p', { text: 'These counts describe saved progress, including repeated tracks across collections. The public library is unchanged until this read finishes.' }));
            deadline('Spotify cooldown ends at', intake.not_before);
            if (!intake.running && intake.pending) {
              const deadlines = [intake.retry_at, intake.not_before].filter((at) => at !== null);
              if (deadlines.length) deadline('Next library pass no earlier than', Math.max(...deadlines));
            }
          }
          if (!observed.last) facts.append(el('p', { text: 'No completed full refresh reported by this daemon yet.' }));
          else {
            const last = observed.last, at = new Date(last.at * 1000);
            facts.append(el('h3', { text: 'Last completed full refresh' }),
              el('time', { datetime: at.toISOString(), text: `${at.toLocaleString()} (browser local time)` }));
            const counts = el('dl', { class: 'library-sync-counts' });
            const fields = [['Tracks read', 'tracks'], ['DJ candidates', 'candidates'],
              ['Classical candidates', 'classical'], ['Retired tracks', 'retired']];
            if (!observed.genre_enrichment) fields.push(['Artist genres pending', 'genre_artists_pending']);
            for (const [label, key] of fields) {
              counts.append(el('dt', { text: label }), el('dd', { text: last[key].toLocaleString() }));
            }
            facts.append(counts);
            if (last.notes.length) facts.append(el('div', { class: 'library-sync-warning' },
              el('strong', { text: 'Not everything was read' }),
              el('ul', {}, ...last.notes.map((note) => el('li', { text: note })))));
            if (!last.candidates) facts.append(el('p', { text: 'No DJ candidates were reported. Check the diagnostics, sign-in and your saved music before starting a DJ.' }));
          }
          if (observed.genre_enrichment) {
            const genres = observed.genre_enrichment;
            facts.append(el('h3', { text: 'Artist genre enrichment' }),
              el('p', { text: genres.requires_refresh ? 'A full library refresh is needed before genre enrichment can continue.'
                : genres.running ? 'A bounded artist genre pass is running on the daemon.'
                  : genres.pending ? 'Artist genres are still incomplete. The daemon resumes in bounded passes without rereading saved music.'
                    : 'Artist genres are complete for the cached library.' }));
            facts.append(el('dl', { class: 'library-sync-counts' },
              el('dt', { text: 'Artist lookups pending' }), el('dd', { text: genres.pending.toLocaleString() }),
              el('dt', { text: 'Completed in the latest pass' }), el('dd', { text: genres.completed.toLocaleString() })));
            if (genres.not_before !== observed.library_intake?.not_before) deadline('Spotify cooldown ends at', genres.not_before);
            if (!genres.running && genres.pending && !genres.requires_refresh) {
              const deadlines = [genres.retry_at, genres.not_before].filter((at) => at !== null);
              if (deadlines.length) deadline('Next genre pass no earlier than', Math.max(...deadlines));
            }
          }
          if (observed.followed_artists) {
            const followed = observed.followed_artists;
            const phase = { reading: 'Reading the artists you follow in a bounded pass.',
              verifying: 'Checking retained followed-artist pages before reconciliation.',
              ready: 'Followed artists are retained and verified. A full library refresh still needs to reconcile them.' };
            facts.append(el('h3', { text: 'Followed artists' }),
              el('p', { text: followed.requires_refresh ? 'A full library refresh is needed before followed-artist intake can continue.'
                : followed.limited ? 'Followed-artist intake reached its cache limit. The retained directory is incomplete; automatic page reads have stopped for this scan.'
                  : followed.running || followed.pending ? phase[followed.phase]
                    : 'Followed-artist directory intake is complete.' }));
            facts.append(el('dl', { class: 'library-sync-counts' },
              el('dt', { text: 'Validated pages retained' }), el('dd', { text: followed.pages.toLocaleString() }),
              el('dt', { text: 'Artists retained' }), el('dd', { text: followed.artists.toLocaleString() }),
              el('dt', { text: 'Pages validated for reconciliation' }), el('dd', { text: followed.verified_pages.toLocaleString() })));
            if (followed.pending && !followed.running && !followed.requires_refresh && followed.phase !== 'ready') {
              facts.append(el('p', { text: 'Progress is saved. The daemon continues this directory in bounded passes without rereading saved music.' }));
            }
            if (followed.not_before !== observed.library_intake?.not_before
                && followed.not_before !== observed.genre_enrichment?.not_before) deadline('Spotify cooldown ends at', followed.not_before);
            if (!followed.running && followed.pending && !followed.requires_refresh && !followed.limited) {
              const deadlines = [followed.retry_at, followed.not_before].filter((at) => at !== null);
              const label = followed.phase === 'ready' ? 'Next library reconciliation no earlier than' : 'Next followed-artist pass no earlier than';
              if (deadlines.length) deadline(label, Math.max(...deadlines));
            }
          }
          if (observed.artist_sampling) {
            const samples = observed.artist_sampling;
            facts.append(el('h3', { text: 'Artist sample discovery' }),
              el('p', { text: samples.requires_refresh ? 'A full library refresh is needed before artist sampling can continue.'
                : samples.limited ? 'Artist sampling reached its cache limit. Automatic sample reads are paused for this refresh cycle.'
                  : samples.running ? 'Reading artist samples in a bounded background pass.'
                    : samples.pending ? 'The daemon continues sampling the remaining artists. Successful reads expand future DJ choices; already queued music stays queued.'
                      : 'Artist sampling is complete for this refresh cycle.' }));
            facts.append(el('dl', { class: 'library-sync-counts' },
              el('dt', { text: 'Artists left to sample' }), el('dd', { text: samples.pending.toLocaleString() }),
              el('dt', { text: 'Artists sampled in this refresh cycle' }), el('dd', { text: samples.completed.toLocaleString() }),
              el('dt', { text: 'Attempted in the latest pass' }), el('dd', { text: samples.attempted.toLocaleString() })));
            if (samples.not_before !== observed.library_intake?.not_before
                && samples.not_before !== observed.genre_enrichment?.not_before
                && samples.not_before !== observed.followed_artists?.not_before) deadline('Spotify cooldown ends at', samples.not_before);
            if (!samples.running && samples.pending && !samples.requires_refresh && !samples.limited) {
              const deadlines = [samples.retry_at, samples.not_before].filter((at) => at !== null);
              if (deadlines.length) deadline('Next artist-sample pass no earlier than', Math.max(...deadlines));
            }
          }
          if (observed.recent_listening) {
            const recent = observed.recent_listening;
            facts.append(el('h3', { text: 'Recent listening on Spotify' }),
              el('p', { text: recent.running ? 'Checking the latest plays so the DJ can avoid music you just heard on another device.'
                : recent.error ? `The latest listening check failed: ${recent.error}. ${recent.checked_at !== null
                  ? 'The last accepted window retains its original timestamp.' : 'No accepted window is available for the current account.'}`
                  : recent.missing_scope ? 'Sign in to Spotify again with the recent-listening grant to enable this check.'
                    : recent.requires_refresh ? 'The current Spotify account needs a full library refresh before its listening can be accepted.'
                      : recent.checked_at !== null ? 'The next DJ selections use the accepted listening observations. Already queued music stays queued.'
                        : 'No recent-listening window has been accepted by this reader yet.' }));
            if (recent.checked_at !== null) {
              deadline('Last accepted listening check', recent.checked_at);
              facts.append(el('dl', { class: 'library-sync-counts' },
                el('dt', { text: 'History rows observed' }), el('dd', { text: recent.received.toLocaleString() }),
                el('dt', { text: 'Distinct track identities accepted' }), el('dd', { text: recent.accepted.toLocaleString() }),
                el('dt', { text: 'Unavailable or non-track rows skipped' }), el('dd', { text: recent.skipped.toLocaleString() })));
            }
            facts.append(el('p', { text: 'Each background check reads at most the latest 50 plays. Older listening between checks can be missed; this is not a complete history import.' }));
            if (recent.more_available) facts.append(el('p', { text: 'This window may omit older plays.' }));
            if (recent.not_before !== observed.library_intake?.not_before
                && recent.not_before !== observed.genre_enrichment?.not_before
                && recent.not_before !== observed.followed_artists?.not_before
                && recent.not_before !== observed.artist_sampling?.not_before) deadline('Spotify cooldown ends at', recent.not_before);
            if (!recent.running) deadline('Next listening check no earlier than', recent.retry_at);
          }
        }
      }
      facts.setAttribute('aria-label', state.fresh ? 'Observed library refresh' : 'Last observed library refresh; may be stale');
      stale.hidden = state.fresh || !state.status;
    }
    const stale = el('p', { class: 'library-sync-warning', hidden: true,
      text: 'Last observation shown. A fresh status read is needed before requesting another refresh.' });
    box.append(el('h2', { text: 'Keep the DJ’s library current' }),
      el('p', { text: 'Reads your Spotify library into the daemon cache. Does not start or skip music. Newly selected DJ picks use the updated cache; already queued works stay queued.' }),
      el('div', { class: 'row' }, check, start, button('Close', () => { control.close(); toggle.focus(); })), status, stale, facts);
    const visibility = () => control.setVisible(!document.hidden);
    const requested = () => { void control.open(); box.focus(); };
    document.addEventListener('fsonos-library-refresh-open', requested);
    document.addEventListener('visibilitychange', visibility); visibility();
    render(control.state);
    return { node: el('div', { class: 'library-sync' }, toggle, box), model: control,
      dispose() {
        document.removeEventListener('fsonos-library-refresh-open', requested);
        document.removeEventListener('visibilitychange', visibility); control.dispose();
      } };
  }
  return { routes, statusOf, model, panel, POLL_MS, PENDING_POLL_MS, GENRE_POLL_MS: PENDING_POLL_MS, MAX_BACKOFF_MS };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = LibrarySyncRemote;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, LibrarySyncRemote.routes);
  const librarySyncRoot = document.getElementById('music-library');
  if (librarySyncRoot) librarySyncRoot.append(LibrarySyncRemote.panel({ call, el }).node);
}
