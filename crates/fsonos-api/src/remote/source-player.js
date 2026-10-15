// Play pasted music through the same policy-checked API as library selections.
'use strict';
const SourcePlayerRemote = (() => {
  const routes = { sourcePlay: ['POST', '/play'] };
  const MAX_BYTES = 2048, MAX_TITLE = 256;
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const problem = (e) => [e?.message || 'The request failed.', e?.hint].filter(Boolean).join(' ');

  function spotify(kind, id) {
    kind = kind.toLowerCase();
    if (!['track', 'album', 'playlist'].includes(kind)) {
      throw new Error('Choose a Spotify track, album or playlist, not an artist, show or episode.');
    }
    if (!/^[a-zA-Z0-9]{22}$/.test(id)) throw new Error('The Spotify id must contain exactly 22 letters and digits. Paste the complete link.');
    return Object.freeze({ uri: `spotify:${kind}:${id}`, kind, container: kind !== 'track' });
  }

  function source(raw) {
    if (typeof raw !== 'string') throw new Error('Paste a Spotify link or an audio stream address.');
    const uri = raw.trim();
    if (!uri || new TextEncoder().encode(uri).length > MAX_BYTES) throw new Error(`Use one complete source address, at most ${MAX_BYTES} bytes.`);
    if (/[\s\p{Cc}]/u.test(uri)) throw new Error('The source contains whitespace or control characters. Paste one complete address.');
    if (/^spotify:/i.test(uri)) {
      const parts = uri.split(':');
      if (parts.length === 3) return spotify(parts[1], parts[2]);
      if (parts.length === 5 && /^user$/i.test(parts[1]) && /^playlist$/i.test(parts[3])) return spotify('playlist', parts[4]);
      throw new Error('Use a Spotify track, album or playlist URI.');
    }
    // Only media addresses: never resolve short links, load pages, or execute a URI.
    const authority = /^https?:\/\/([^/?#]+)/i.exec(uri);
    if (!authority || authority[1].includes('@') || uri.includes('\\')) {
      throw new Error('Use a complete HTTP or HTTPS audio address without embedded sign-in details.');
    }
    let url;
    try { url = new URL(uri); } catch (_) { throw new Error('The audio address is malformed.'); }
    if (!url.hostname || !['http:', 'https:'].includes(url.protocol)) throw new Error('Use an HTTP or HTTPS audio address.');
    const host = url.hostname.toLowerCase().replace(/\.$/, '');
    if (host === 'spotify.link' || host.endsWith('.spotify.link')) {
      throw new Error('Open the shortened link yourself and paste the complete open.spotify.com address.');
    }
    if (host === 'open.spotify.com' || host === 'play.spotify.com') {
      const segments = url.pathname.split('/').filter(Boolean);
      if (segments[0]?.startsWith('intl-')) segments.shift();
      if (segments[0] === 'embed') segments.shift();
      if (segments.length === 2) return spotify(...segments);
      if (segments.length === 4 && segments[0] === 'user' && segments[2] === 'playlist') return spotify('playlist', segments[3]);
      throw new Error('Paste a link to one Spotify track, album or playlist.');
    }
    // Preserve escaping, query parameters and letter case, which may be signed.
    return Object.freeze({ uri, kind: 'stream', container: false });
  }

  function groups(value) {
    if (!Array.isArray(value)) throw new Error('The room list is unreadable. Refresh rooms.');
    const seen = new Set();
    return value.map((zone) => {
      if (!zone || !text(zone.household) || !text(zone.coordinator_room)
          || !Array.isArray(zone.members) || !zone.members.length
          || !zone.members.every(text) || !zone.members.includes(zone.coordinator_room)) throw new Error('The room list is incomplete. Refresh rooms.');
      for (const room of zone.members) {
        const key = JSON.stringify([zone.household, room]);
        if (seen.has(key)) throw new Error('The room list assigns a room more than once. Refresh rooms.');
        seen.add(key);
      }
      const target = `${zone.coordinator_room}@${zone.household}`;
      if ([...target].length > 128) throw new Error('The group address exceeds the supported length.');
      return Object.freeze({ target, household: zone.household, coordinator: zone.coordinator_room,
        label: `${zone.members.join(' + ')} (${zone.household})`,
        signature: JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) });
    });
  }

  function playback(value, group) {
    if (!value || groups([value.zone])[0].signature !== group.signature
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)) {
      throw new Error('The playback group changed. Refresh rooms.');
    }
    const track = value.track;
    if (track != null && (!text(track.uri) || (track.queue_position != null
        && (!Number.isInteger(track.queue_position) || track.queue_position < 1 || track.queue_position > 0xffffffff)))) {
      throw new Error('The current source is unreadable. Read it again before replacing playback.');
    }
    return Object.freeze({ signature: JSON.stringify([value.transport_state, track?.uri ?? null, track?.queue_position ?? null]),
      label: track ? `${track.title || track.uri} (${value.transport_state})` : `No current track (${value.transport_state})` });
  }

  function outcome(value) {
    if (!value || !text(value.done) || typeof value.changed !== 'boolean'
        || (value.notes != null && (!Array.isArray(value.notes)
          || value.notes.some((note) => !note || typeof note.detail !== 'string')))) throw new Error('The playback reply is incomplete.');
    return [value.done, ...(value.notes || []).map((note) => note.detail)].filter(Boolean).join(' ');
  }

  function model({ call, changed = () => {}, execute = (_, work) => work() }) {
    let epoch = 0, disposed = false;
    const state = { open: false, groups: [], target: '', input: '', title: '', pending: null,
      reading: false, writing: false, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => state.groups.find((group) => group.target === state.target);
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.reading = false; };

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      const prior = selected();
      invalidate(); const version = epoch;
      state.reading = true; state.message = ''; publish();
      try {
        const fresh = groups(await call('zones'));
        if (!current(version)) return false;
        state.groups = fresh;
        if (!prior || !fresh.some((group) => group.signature === prior.signature)) state.target = '';
        if (!fresh.length) state.message = 'No groups discovered. Refresh after the speakers are available.';
        return true;
      } catch (error) {
        if (current(version)) { state.groups = []; state.target = ''; state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    function edit(field, value) {
      if (disposed || state.writing || !['target', 'input', 'title'].includes(field) || typeof value !== 'string') return false;
      if (field === 'target' && !state.groups.some((group) => group.target === value)) return false;
      invalidate(); state[field] = value; state.message = ''; publish(); return true;
    }

    async function inspect(group) {
      const fresh = groups(await call('zones'));
      if (!fresh.some((entry) => entry.signature === group.signature)) throw new Error('The destination group changed. Refresh rooms and review again.');
      return playback(await call('state', { params: { room: group.target } }), group);
    }

    async function review() {
      if (disposed || !state.open || state.writing) return false;
      invalidate();
      const version = epoch, group = selected();
      let item, title;
      try {
        if (!group) throw new Error('Choose the group that should play this source.');
        item = source(state.input);
        title = [...state.title.trim()].slice(0, MAX_TITLE).join('');
        if (/[\p{Cc}]/u.test(title)) throw new Error('The display title contains control characters.');
      } catch (error) { state.message = problem(error); publish(); return false; }
      state.reading = true; state.message = ''; publish();
      try {
        const before = await inspect(group);
        if (!current(version)) return false;
        const body = { zone: group.target, source_uri: item.uri };
        if (title) body.title = title;
        state.pending = Object.freeze({ group, item, title, before, body: Object.freeze(body) });
        return true;
      } catch (error) { if (current(version)) state.message = problem(error); return false; }
      finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function confirm(pending) {
      if (disposed || !state.open || state.writing || state.reading || !pending || pending !== state.pending) return false;
      const version = ++epoch;
      state.pending = null; state.writing = true; state.message = 'Checking the destination before playback…'; state.result = null; publish();
      let sent = false, accepted = false;
      try {
        await execute(pending.group, async () => {
          if (!current(version)) return;
          const before = await inspect(pending.group);
          if (!current(version)) return;
          if (before.signature !== pending.before.signature) throw new Error('Playback changed since your review. Review the current source before replacing it.');
          sent = true;
          const detail = outcome(await call('sourcePlay', { body: pending.body }));
          accepted = true;
          // A dispatched outcome belongs to this operation, even if its view closed.
          state.result = Object.freeze({ status: 'accepted', detail });
        });
        if (!sent && current(version)) state.message = 'No playback command was sent. Review the source again.';
      } catch (error) {
        state.result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent',
          detail: `${problem(error)} ${sent
            ? 'Playback may have changed or may still finish. Check the room before another action; this request was not repeated.'
            : 'No playback command was sent.'}` });
      } finally {
        state.writing = false;
        if (current(version)) state.message = '';
        publish();
      }
      return accepted;
    }

    return { state, refresh, edit, review, confirm,
      cancel() { if (!state.writing && !disposed) { invalidate(); state.message = ''; publish(); } },
      close() { state.open = false; invalidate(); state.message = ''; publish(); },
      dispose() { disposed = true; state.open = false; invalidate(); },
    };
  }

  function panel({ call, el, execute, document: doc = document }) {
    const target = el('select', { 'aria-label': 'Destination group for pasted music' });
    const input = el('input', { type: 'text', 'aria-label': 'Spotify link or audio stream address',
      placeholder: 'Spotify link, Spotify URI, or HTTP audio address', autocomplete: 'off', spellcheck: 'false' });
    const title = el('input', { type: 'text', 'aria-label': 'Optional display title', placeholder: 'Optional display title', maxlength: MAX_TITLE });
    const refresh = el('button', { type: 'button', text: 'Refresh rooms' });
    const review = el('button', { type: 'button', text: 'Review playback' });
    const message = el('p', { role: 'status', 'aria-live': 'polite' });
    const result = el('p', { role: 'status', 'aria-live': 'polite' });
    const consent = el('div', { class: 'source-review', hidden: true });
    const details = el('details', { class: 'zone source-player' }, el('summary', { text: 'Play a link or stream' }),
      el('div', {}, el('p', { text: 'Play a Spotify track, album or playlist without syncing it first, or send a direct audio stream to the speakers.' }),
        el('label', {}, 'Destination ', target), el('label', {}, 'Source ', input), el('label', {}, 'Title ', title),
        el('div', { class: 'row' }, refresh, review), message, consent, result));
    const node = el('section', { class: 'scenes', 'aria-label': 'Play a link or stream' }, details);
    let shownGroups = null, shownReview = null, confirmButton = null;
    const control = model({ call, execute, changed: render });
    function render(state) {
      const busy = state.reading || state.writing;
      if (shownGroups !== state.groups) {
        shownGroups = state.groups;
        target.replaceChildren(el('option', { value: '', text: 'Choose a group…', disabled: true }));
        for (const group of state.groups) target.append(el('option', { value: group.target, text: group.label }));
      }
      target.value = state.target;
      for (const [field, element] of [['input', input], ['title', title]]) if (element.value !== state[field]) element.value = state[field];
      for (const element of [target, input, title]) element.disabled = state.writing;
      refresh.disabled = busy; review.disabled = busy || !state.target || !state.input.trim();
      message.textContent = state.message || (state.reading ? 'Reading the destination…' : '');
      result.textContent = state.result?.detail || '';
      if (shownReview !== state.pending) {
        shownReview = state.pending; consent.replaceChildren(); consent.hidden = !shownReview;
        if (shownReview) {
          const pending = shownReview;
          confirmButton = el('button', { type: 'button', text: 'Play now' });
          const cancel = el('button', { type: 'button', text: 'Cancel' });
          confirmButton.addEventListener('click', () => { void control.confirm(pending); });
          cancel.addEventListener('click', () => control.cancel());
          consent.append(el('p', { text: `Play ${pending.title || pending.item.uri} in ${pending.group.label}?` }),
            el('p', { text: `Current playback: ${pending.before.label}.` }),
            el('p', { text: pending.item.container
              ? 'This replaces the group’s queue and starts the album or playlist.'
              : 'This replaces the group’s current source and starts playback.' }),
            el('p', { class: 'source-help', text: 'The destination is checked again before sending. Another controller can still change it afterward. The speakers must support and reach the source.' }),
            el('div', { class: 'row' }, confirmButton, cancel));
        }
      }
      if (confirmButton) confirmButton.disabled = busy || !state.pending;
    }
    target.addEventListener('change', () => control.edit('target', target.value));
    input.addEventListener('input', () => control.edit('input', input.value));
    title.addEventListener('input', () => control.edit('title', title.value));
    refresh.addEventListener('click', () => { void control.refresh(); });
    review.addEventListener('click', () => { void control.review(); });
    details.addEventListener('toggle', () => { if (details.open) void control.refresh(); else control.close(); });
    const hide = () => { if (doc.hidden) control.close(); else if (details.open) void control.refresh(); };
    doc.addEventListener('visibilitychange', hide);
    render(control.state);
    return { node, ...control, dispose() { doc.removeEventListener('visibilitychange', hide); control.dispose(); } };
  }

  return { source, groups, playback, model, panel, routes, MAX_BYTES, MAX_TITLE };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SourcePlayerRemote;
else {
  Object.assign(ROUTES, SourcePlayerRemote.routes);
  const host = document.getElementById('households');
  if (host) host.before(SourcePlayerRemote.panel({ call, el,
    async execute(group, work) {
      let parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
      if (!parts) {
        await loadZones();
        parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
      }
      if (!parts || !beginAction(parts)) throw new Error('The destination changed or another playback action is busy. Refresh rooms and review again.');
      try { return await work(); } finally { endAction(parts); }
    },
  }).node);
}
