// Play links and saved Sonos playlists through the policy-checked playback API.
'use strict';
const SourcePlayerRemote = (() => {
  const routes = { sourcePlay: ['POST', '/play'], sourceQueue: ['POST', '/queue'],
    sourceQueueState: ['GET', '/zones/{room}/queue'],
    sourcePlaylists: ['GET', '/sonos/playlists'], sourcePlaylistPlay: ['POST', '/sonos/playlists/play'],
    sourcePlaylistContents: ['GET', '/sonos/playlists/contents'], sourcePlaylistTrack: ['POST', '/sonos/playlists/track'] };
  const modes = Object.freeze({ play: 'Play now', next: 'Queue next', end: 'Queue at end' });
  const MAX_BYTES = 2048, MAX_TITLE = 256;
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const problem = (e) => [e?.message || 'The request failed.', e?.hint].filter(Boolean).join(' ');

  function spotify(kind, id) {
    kind = kind.toLowerCase();
    if (!['track', 'album', 'playlist', 'artist', 'episode', 'show'].includes(kind)) {
      throw new Error('Choose a Spotify track, album, playlist, artist, episode or show.');
    }
    if (!/^[a-zA-Z0-9]{22}$/.test(id)) throw new Error('The Spotify id must contain exactly 22 letters and digits. Paste the complete link.');
    return Object.freeze({ uri: `spotify:${kind}:${id}`, kind, container: ['album', 'playlist', 'artist', 'show'].includes(kind) });
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
      throw new Error('Use a Spotify track, album, playlist, artist, episode or show URI.');
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
      throw new Error('Paste a link to one Spotify track, album, playlist, artist, episode or show.');
    }
    // Preserve escaping, query parameters and letter case, which may be signed.
    return Object.freeze({ uri, kind: 'stream', container: false });
  }

  // Import only a source and an optional display title. A historical room,
  // queue position or action must never implicitly select the next command.
  function offeredSource(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)
        || Object.keys(value).some((key) => !['source_uri', 'title'].includes(key))) {
      throw new Error('The source handoff is incomplete or contains a playback destination.');
    }
    const item = source(value.source_uri);
    if (value.title !== undefined && (typeof value.title !== 'string'
        || [...value.title].length > MAX_TITLE || /[\p{Cc}]/u.test(value.title))) {
      throw new Error('The source display title is invalid.');
    }
    return Object.freeze({ input: item.uri, title: value.title?.trim() || '' });
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

  function queue(value, group) {
    const uint = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
    if (!value || value.zone !== group.coordinator || !uint(value.total)
        || !text(value.revision) || value.revision.length > 512
        || !Array.isArray(value.items) || value.items.length !== Math.min(1, value.total)
        || (value.items.length && value.items[0]?.position !== 1)
        || (value.playing != null && (!uint(value.playing) || value.playing < 1 || value.playing > value.total))) {
      throw new Error('The queue snapshot is incomplete. Refresh and review the addition again.');
    }
    return Object.freeze({ total: value.total, revision: value.revision, playing: value.playing ?? null });
  }

  function queued(value, group) {
    const positive = (n) => Number.isInteger(n) && n > 0 && n <= 0xffffffff;
    if (!value || value.zone !== group.coordinator || !text(value.done)
        || !positive(value.first) || !positive(value.count)
        || value.first + value.count - 1 > 0xffffffff || typeof value.playing_queue !== 'boolean') {
      throw new Error('The queue addition was not confirmed by a complete response.');
    }
    return `${value.done} ${value.count} track(s) added starting at position ${value.first}.${value.playing_queue
      ? ' Existing playback was not restarted.' : ' The group is not using its queue; adding music did not start it.'}`;
  }

  function outcome(value) {
    if (!value || !text(value.done) || typeof value.changed !== 'boolean'
        || (value.notes != null && (!Array.isArray(value.notes)
          || value.notes.some((note) => !note || typeof note.detail !== 'string')))) throw new Error('The playback reply is incomplete.');
    return [value.done, ...(value.notes || []).map((note) => note.detail)].filter(Boolean).join(' ');
  }

  function prepare(raw, title, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Queue next or Queue at end.');
    const item = source(raw);
    if (mode !== 'play' && item.kind === 'stream') {
      throw new Error('Only Spotify sources can be queued here. Choose Play now for an audio stream.');
    }
    if (typeof title !== 'string') throw new Error('The display title is invalid.');
    title = [...title.trim()].slice(0, MAX_TITLE).join('');
    if (/[\p{Cc}]/u.test(title)) throw new Error('The display title contains control characters.');
    return Object.freeze({ item, title, mode });
  }

  function playlists(value) {
    if (!Array.isArray(value) || value.some(item => !item || typeof item.id !== 'string' || !/^SQ:[0-9]+$/.test(item.id)
        || typeof item.title !== 'string') || new Set(value.map(item => item.id)).size !== value.length) {
      throw new Error('The saved Sonos playlist list is unreadable. Refresh playlists.');
    }
    return Object.freeze(value.map(item => Object.freeze({ id: item.id, title: item.title })));
  }

  function preparePlaylist(id, catalog, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Queue next or Queue at end.');
    const selected = catalog.find(item => item.id === id);
    if (!selected) throw new Error('Choose a saved Sonos playlist for this group.');
    return Object.freeze({ item: Object.freeze({ ...selected, kind: 'sonos_playlist', container: true }),
      title: selected.title || selected.id, mode });
  }

  function verifyPlaylist(value, item) {
    const fresh = playlists(value).find(playlist => playlist.id === item.id);
    if (!fresh || fresh.title !== item.title) {
      throw new Error('The selected Sonos playlist changed or disappeared. Refresh playlists and review again.');
    }
  }

  function contentsRequest(page, history, direction, playlist) {
    if (!['refresh', 'next', 'previous'].includes(direction)) return null;
    if (direction !== 'refresh' && (!page || page.playlist.id !== playlist)) return null;
    const offset = direction === 'refresh' ? 0 : direction === 'next' ? page.next_offset : history.at(-1);
    if (offset === null || offset === undefined) return null;
    return { query: { playlist, offset, limit: 100,
      ...(direction === 'refresh' ? {} : { expected_revision: page.revision }) },
      history: direction === 'refresh' ? [] : direction === 'next' ? [...history, page.offset] : history.slice(0, -1) };
  }

  function playlistContents(value, query, selected, group) {
    const fail = () => { throw new Error('The saved playlist contents changed or are unreadable. Read tracks again.'); };
    const uint = n => Number.isInteger(n) && n >= 0 && n <= 0xffffffff;
    if (!value || !group || value.zone !== group.coordinator || !value.playlist || value.playlist.id !== query.playlist
        || value.playlist.title !== selected.title || typeof value.revision !== 'string' || !value.revision
        || value.revision.length > 4096 || /[\p{Cc}]/u.test(value.revision)
        || (query.expected_revision && value.revision !== query.expected_revision)
        || !uint(value.total) || value.offset !== query.offset || !Array.isArray(value.items)
        || value.items.length > query.limit || value.offset + value.items.length > value.total) fail();
    const next = value.offset + value.items.length;
    if ((value.next_offset ?? null) !== (next < value.total ? next : null)
        || (!value.items.length && next < value.total)) fail();
    const items = value.items.map((item, index) => {
      if (!item || item.position !== value.offset + index + 1 || typeof item.title !== 'string'
          || ['creator', 'album', 'uri', 'art_uri'].some(key => item[key] != null && typeof item[key] !== 'string')
          || (item.duration_secs != null && !uint(item.duration_secs))) fail();
      return Object.freeze({ ...item });
    });
    return Object.freeze({ ...value, playlist: Object.freeze({ ...value.playlist }),
      next_offset: value.next_offset ?? null, items: Object.freeze(items) });
  }

  function contentsSummary(page) {
    if (!page) return 'Read the saved tracks without changing playback.';
    if (!page.total) return 'This saved Sonos playlist is empty.';
    return `Tracks ${page.offset + 1}–${page.offset + page.items.length} of ${page.total} · ${page.playlist.title || page.playlist.id}`;
  }

  function preparePlaylistTrack(page, position, mode) {
    if (!Object.hasOwn(modes, mode)) throw new Error('Choose Play now, Play next or Add to queue.');
    const track = page?.items.find(item => item.position === position);
    if (!track || !Number.isInteger(position) || position < 1 || position > 100000
        || !text(track.uri) || track.uri.trim() === 'NOT_IMPLEMENTED') {
      throw new Error('This saved occurrence has no playable resource. Read the tracks again or choose another track.');
    }
    return Object.freeze({ item: Object.freeze({ kind: 'sonos_playlist_track', container: false,
      id: page.playlist.id, playlistTitle: page.playlist.title, position, revision: page.revision,
      title: track.title, uri: track.uri }), title: track.title || 'Untitled track', mode });
  }

  function trackQuery(item, target) {
    return { zone: target, playlist: item.id, offset: item.position - 1, limit: 1, expected_revision: item.revision };
  }

  function verifyPlaylistTrack(value, item, group) {
    const page = playlistContents(value, trackQuery(item, group.target), { title: item.playlistTitle }, group);
    const track = page.items[0];
    if (page.items.length !== 1 || track.position !== item.position || track.uri !== item.uri || track.title !== item.title) {
      throw new Error('The selected saved track changed. Read tracks and choose it again.');
    }
  }

  function trackOutcome(value, reviewed) {
    const item = reviewed.item, mode = reviewed.mode === 'end' ? 'append' : reviewed.mode;
    if (!value || value.zone !== reviewed.group.coordinator || value.playlist !== item.id
        || value.position !== item.position || value.mode !== mode || value.title !== item.title
        || (mode !== 'play' && value.count !== 1)) {
      throw new Error('The saved track command was not confirmed for the reviewed occurrence.');
    }
    const detail = outcome(value);
    return mode === 'play' ? detail : [queued(value, reviewed.group), ...(value.notes || []).map(note => note.detail)].join(' ');
  }

  function renderContents(list, page, doc, { review, disabled = false } = {}) {
    list.replaceChildren();
    for (const item of page?.items || []) {
      const row = doc.createElement('li'); row.value = item.position;
      const title = doc.createElement('strong'); title.textContent = item.title || 'Untitled track'; row.append(title);
      const detail = [item.creator, item.album].filter(Boolean);
      if (item.duration_secs != null) detail.push(Math.floor(item.duration_secs / 60) + ':' + String(item.duration_secs % 60).padStart(2, '0'));
      if (detail.length) { const text = doc.createElement('span'); text.textContent = detail.join(' · '); row.append(text); }
      if (review) {
        const actions = doc.createElement('div'); actions.className = 'source-track-actions';
        for (const [mode, label] of [['play', 'Play now'], ['next', 'Play next'], ['end', 'Add to queue']]) {
          const button = doc.createElement('button'); button.type = 'button'; button.textContent = label;
          button.disabled = disabled || !text(item.uri) || item.uri.trim() === 'NOT_IMPLEMENTED';
          button.setAttribute('aria-label', `${label}: track ${item.position}, ${item.title || 'Untitled track'}`);
          button.addEventListener('click', () => { if (!button.disabled) void review(item.position, mode, page); });
          actions.append(button);
        }
        row.append(actions);
      }
      list.append(row);
    }
  }

  function commandBody(choice, target) {
    if (choice.item.kind === 'sonos_playlist_track') return { zone: target, playlist: choice.item.id,
      position: choice.item.position, expected_revision: choice.item.revision, mode: choice.mode === 'end' ? 'append' : choice.mode };
    return { zone: target, ...(choice.item.kind === 'sonos_playlist'
      ? { playlist: choice.item.id }
      : { source_uri: choice.item.uri, ...(choice.title ? { title: choice.title } : {}) }),
      ...(choice.mode === 'play' ? {} : { next: choice.mode === 'next' }) };
  }

  function model({ call, changed = () => {}, execute = (_, work) => work() }) {
    let epoch = 0, disposed = false;
    const state = { open: false, groups: [], target: '', sourceType: 'link', input: '', title: '', mode: 'play', pending: null,
      playlists: [], playlist: '', playlistTarget: '', playlistStatus: 'idle', contents: null, contentsHistory: [],
      reading: false, writing: false, message: '', result: null };
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Presentation only. */ } } };
    const selected = () => state.groups.find((group) => group.target === state.target);
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => {
      ++epoch; state.pending = null; state.reading = false;
      if (state.playlistStatus === 'loading') state.playlistStatus = 'idle';
    };
    const clearContents = () => { state.contents = null; state.contentsHistory = []; };
    const clearPlaylists = () => { state.playlists = []; state.playlist = ''; state.playlistTarget = ''; state.playlistStatus = 'idle'; clearContents(); };

    async function refresh() {
      if (disposed) return false;
      state.open = true;
      if (state.writing) { publish(); return false; }
      const prior = selected();
      invalidate(); clearPlaylists(); const version = epoch;
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
      if (disposed || state.writing || !['target', 'sourceType', 'playlist', 'input', 'title', 'mode'].includes(field) || typeof value !== 'string') return false;
      if (field === 'target' && !state.groups.some((group) => group.target === value)) return false;
      if (field === 'mode' && !Object.hasOwn(modes, value)) return false;
      if (field === 'sourceType' && !['link', 'playlist'].includes(value)) return false;
      if (field === 'playlist' && (state.playlistTarget !== state.target || !state.playlists.some(item => item.id === value))) return false;
      invalidate();
      if (field === 'target' || field === 'sourceType') clearPlaylists();
      if (field === 'playlist') clearContents();
      state[field] = value; state.message = ''; publish(); return true;
    }

    async function loadPlaylists() {
      const group = selected();
      if (disposed || !state.open || state.writing || state.sourceType !== 'playlist' || !group) return false;
      invalidate(); clearPlaylists(); const version = epoch;
      state.reading = true; state.playlistStatus = 'loading'; state.message = 'Reading saved Sonos playlists…'; publish();
      try {
        const catalog = playlists(await call('sourcePlaylists', { query: { zone: group.target } }));
        if (!current(version)) return false;
        state.playlists = catalog; state.playlistTarget = group.target; state.playlistStatus = 'ready'; state.message = '';
        return true;
      } catch (error) {
        if (current(version)) { state.playlistStatus = 'error'; state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function loadContents(direction = 'refresh') {
      const group = selected(), item = state.playlists.find(item => item.id === state.playlist);
      if (disposed || !state.open || state.writing || state.reading || state.sourceType !== 'playlist'
          || !group || !item || state.playlistStatus !== 'ready' || state.playlistTarget !== group.target) return false;
      const request = contentsRequest(state.contents, state.contentsHistory, direction, item.id);
      if (!request) return false;
      invalidate(); const version = epoch;
      state.reading = true; state.message = 'Reading saved tracks…'; publish();
      try {
        const value = await call('sourcePlaylistContents', { query: { zone: group.target, ...request.query } });
        if (!current(version)) return false;
        state.contents = playlistContents(value, request.query, item, group); state.contentsHistory = request.history;
        state.message = ''; return true;
      } catch (error) {
        if (current(version)) { clearContents(); state.message = problem(error); }
        return false;
      } finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function offer(value) {
      if (disposed) return false;
      let incoming;
      try { incoming = offeredSource(value); }
      catch (error) { state.message = problem(error); publish(); return false; }
      state.open = true;
      if (state.writing || state.reading || state.pending || state.input.trim() || state.title.trim() || state.playlist) {
        state.message = 'Finish the current source action, or explicitly clear its draft before importing another source.';
        publish(); return false;
      }
      invalidate(); state.input = incoming.input; state.title = incoming.title;
      state.target = ''; state.mode = 'play'; state.sourceType = 'link'; clearPlaylists();
      // Refresh only the selectable destinations. Review and confirmation remain
      // separate user actions, and share the existing write coordination lane.
      return refresh();
    }

    function clearDraft() {
      if (disposed || state.writing) return false;
      invalidate(); clearPlaylists(); state.input = ''; state.title = ''; state.target = ''; state.mode = 'play'; state.sourceType = 'link';
      state.message = 'Source draft cleared. No playback command was sent.';
      // A previous uncertain outcome is still useful evidence, not erased here.
      publish(); return true;
    }

    async function inspect(group, mode, item) {
      const fresh = groups(await call('zones'));
      if (!fresh.some((entry) => entry.signature === group.signature)) throw new Error('The destination group changed. Refresh rooms and review again.');
      if (item.kind === 'sonos_playlist') verifyPlaylist(await call('sourcePlaylists', { query: { zone: group.target } }), item);
      if (item.kind === 'sonos_playlist_track') {
        verifyPlaylistTrack(await call('sourcePlaylistContents', { query: trackQuery(item, group.target) }), item, group);
      }
      const before = playback(await call('state', { params: { room: group.target } }), group);
      if (mode === 'play' && !item.container) return before;
      const snapshot = queue(await call('sourceQueueState', {
        params: { room: group.target }, query: { offset: 0, limit: 1 },
      }), group);
      // Appending tolerates track advances; queue-next and replacement bind playback.
      return Object.freeze({ ...before, queue: snapshot, signature: JSON.stringify([
        snapshot.revision, snapshot.total,
        ...(mode !== 'end' ? [snapshot.playing, before.signature] : []),
      ]) });
    }

    async function review(choice = null) {
      if (disposed || !state.open || state.writing || state.reading) return false;
      invalidate();
      const version = epoch, group = selected(), mode = choice?.mode || state.mode;
      let item, title;
      try {
        if (!group) throw new Error('Choose the group that should play this source.');
        if (state.sourceType === 'playlist' && (state.playlistStatus !== 'ready' || state.playlistTarget !== group.target)) {
          throw new Error('Read the saved Sonos playlists for this group first.');
        }
        ({ item, title } = choice || (state.sourceType === 'playlist'
          ? preparePlaylist(state.playlist, state.playlists, mode) : prepare(state.input, state.title, mode)));
      } catch (error) { state.message = problem(error); publish(); return false; }
      state.reading = true; state.message = ''; publish();
      try {
        const before = await inspect(group, mode, item);
        if (!current(version)) return false;
        const body = commandBody({ item, title, mode }, group.target);
        state.pending = Object.freeze({ group, item, title, mode, before, body: Object.freeze(body) });
        return true;
      } catch (error) { if (current(version)) state.message = problem(error); return false; }
      finally { if (current(version)) { state.reading = false; publish(); } }
    }

    async function reviewTrack(position, mode, page = state.contents) {
      if (disposed || !state.open || state.writing || state.reading || state.sourceType !== 'playlist'
          || !page || page !== state.contents || page.playlist.id !== state.playlist) return false;
      let choice;
      try { choice = preparePlaylistTrack(page, position, mode); }
      catch (error) { invalidate(); state.message = problem(error); publish(); return false; }
      return review(choice);
    }

    async function confirm(pending) {
      if (disposed || !state.open || state.writing || state.reading || !pending || pending !== state.pending) return false;
      const version = ++epoch;
      state.pending = null; state.writing = true; state.message = 'Checking the destination…'; state.result = null; publish();
      let sent = false, accepted = false;
      try {
        await execute(pending.group, async () => {
          if (!current(version)) return;
          const before = await inspect(pending.group, pending.mode, pending.item);
          if (!current(version)) return;
          if (before.signature !== pending.before.signature) throw new Error('Playback or the queue changed since your review. Review the current destination again.');
          sent = true;
          const route = pending.item.kind === 'sonos_playlist_track' ? 'sourcePlaylistTrack'
            : pending.mode === 'play' ? pending.item.kind === 'sonos_playlist' ? 'sourcePlaylistPlay' : 'sourcePlay' : 'sourceQueue';
          const response = await call(route, { body: pending.body });
          const detail = pending.item.kind === 'sonos_playlist_track' ? trackOutcome(response, pending)
            : pending.mode === 'play' ? outcome(response) : queued(response, pending.group);
          accepted = true;
          // A dispatched outcome belongs to this operation, even if its view closed.
          state.result = Object.freeze({ status: 'accepted', detail });
        });
        if (!sent && current(version)) state.result = Object.freeze({ status: 'not_sent', detail: 'No command was sent. Review the source again.' });
      } catch (error) {
        state.result = Object.freeze({ status: sent ? 'uncertain' : 'not_sent',
          detail: `${problem(error)} ${sent
            ? 'Playback or the queue may have changed, or the request may still finish. Check the room before another action; this request was not repeated.'
            : 'No playback command was sent.'}` });
      } finally {
        state.writing = false;
        if (current(version)) state.message = '';
        publish();
      }
      return accepted;
    }

    return { state, refresh, edit, loadPlaylists, loadContents, review, reviewTrack, confirm, offer, clearDraft,
      cancel() { if (!state.writing && !disposed) { invalidate(); state.message = ''; publish(); } },
      close() { state.open = false; invalidate(); state.message = ''; publish(); },
      dispose() { disposed = true; state.open = false; invalidate(); },
    };
  }

  function panel({ call, el, execute, document: doc = document }) {
    const target = el('select', { 'aria-label': 'Destination group for music' });
    const sourceType = el('select', { 'aria-label': 'Music source' },
      el('option', { value: 'link', text: 'Link or stream' }), el('option', { value: 'playlist', text: 'Saved Sonos playlist' }));
    const playlist = el('select', { 'aria-label': 'Saved Sonos playlist' });
    const refreshPlaylists = el('button', { type: 'button', text: 'Refresh playlists' });
    const playlistHelp = el('p', { class: 'source-help', role: 'status' });
    const readContents = el('button', { type: 'button', text: 'Read tracks' });
    const previousContents = el('button', { type: 'button', text: '← Previous tracks' });
    const nextContents = el('button', { type: 'button', text: 'Next tracks →' });
    const contentsHelp = el('p', { class: 'source-help', role: 'status', 'aria-live': 'polite' });
    const contentsList = el('ol', { class: 'source-track-list', 'aria-label': 'Saved playlist tracks' });
    const contentsFields = el('div', { class: 'source-contents', hidden: true },
      el('div', { class: 'row' }, readContents, previousContents, nextContents), contentsHelp, contentsList);
    const playlistFields = el('div', { class: 'source-catalog', hidden: true },
      el('label', {}, 'Saved Sonos playlist ', playlist), refreshPlaylists, playlistHelp, contentsFields);
    const input = el('input', { type: 'text', 'aria-label': 'Spotify link or audio stream address',
      placeholder: 'Spotify link, Spotify URI, or HTTP audio address', autocomplete: 'off', spellcheck: 'false' });
    const mode = el('select', { 'aria-label': 'Play now or add to queue' });
    for (const [value, label] of Object.entries(modes)) mode.append(el('option', { value, text: label }));
    const title = el('input', { type: 'text', 'aria-label': 'Optional display title', placeholder: 'Optional display title', maxlength: MAX_TITLE });
    const refresh = el('button', { type: 'button', text: 'Refresh rooms' });
    const review = el('button', { type: 'button', text: 'Review playback' });
    const clear = el('button', { type: 'button', text: 'Clear source draft' });
    const message = el('p', { role: 'status', 'aria-live': 'polite' });
    const result = el('p', { role: 'status', 'aria-live': 'polite' });
    const consent = el('div', { class: 'source-review', hidden: true, tabindex: '-1', 'aria-label': 'Review selected playback' });
    const inputField = el('label', {}, 'Link or stream ', input), titleField = el('label', {}, 'Title ', title);
    const details = el('details', { class: 'zone source-player' }, el('summary', { text: 'Play a link or Sonos playlist' }),
      el('div', {}, el('p', { text: 'Choose a saved Sonos playlist from your speakers, paste a Spotify link, or send a direct audio stream. Review the destination and action before playing or adding music to the queue.' }),
        el('label', {}, 'Destination ', target), el('label', {}, 'Music source ', sourceType), inputField, titleField, playlistFields,
        el('label', {}, 'Action ', mode), el('div', { class: 'row' }, refresh, review, clear), message, consent, result));
    const node = el('section', { class: 'scenes', 'aria-label': 'Play a link or Sonos playlist' }, details);
    let shownGroups = null, shownPlaylists = null, shownPlaylistStatus = '', shownReview = null, shownContents = null, shownContentsBusy = null, confirmButton = null;
    const control = model({ call, execute, changed: render });
    function render(state) {
      const busy = state.reading || state.writing;
      if (shownGroups !== state.groups) {
        shownGroups = state.groups;
        target.replaceChildren(el('option', { value: '', text: 'Choose a group…', disabled: true }));
        for (const group of state.groups) target.append(el('option', { value: group.target, text: group.label }));
      }
      target.value = state.target; mode.value = state.mode; sourceType.value = state.sourceType;
      const saved = state.sourceType === 'playlist';
      inputField.hidden = saved; titleField.hidden = saved; playlistFields.hidden = !saved;
      if (shownPlaylists !== state.playlists || shownPlaylistStatus !== state.playlistStatus) {
        shownPlaylists = state.playlists; shownPlaylistStatus = state.playlistStatus;
        const caption = state.playlistStatus === 'loading' ? 'Loading playlists…'
          : state.playlistStatus === 'ready' ? state.playlists.length ? 'Choose a playlist…' : 'No saved Sonos playlists'
            : 'Refresh playlists for this group';
        playlist.replaceChildren(el('option', { value: '', text: caption, disabled: true }));
        for (const item of state.playlists) playlist.append(el('option', { value: item.id, text: item.title || item.id }));
      }
      playlist.value = state.playlist;
      playlist.disabled = state.writing || state.playlistStatus !== 'ready' || !state.playlists.length;
      refreshPlaylists.disabled = busy || !state.target;
      playlistHelp.textContent = state.playlistStatus === 'ready' ? state.playlists.length
        ? 'Saved on the speakers in this household. Read its tracks before choosing how to play it.'
        : 'This household has no saved Sonos playlists. Save a playlist in Sonos, then refresh here.'
        : state.target ? 'Read the saved playlists for this destination.' : 'Choose a destination group first.';
      contentsFields.hidden = !state.playlist || state.playlistStatus !== 'ready';
      readContents.disabled = busy || !state.playlist;
      readContents.textContent = state.contents ? 'Refresh tracks' : 'Read tracks';
      previousContents.hidden = !state.contentsHistory.length; previousContents.disabled = busy;
      nextContents.hidden = state.contents?.next_offset == null; nextContents.disabled = busy;
      contentsHelp.textContent = contentsSummary(state.contents);
      if (shownContents !== state.contents || shownContentsBusy !== busy) {
        shownContents = state.contents; shownContentsBusy = busy;
        renderContents(contentsList, state.contents, doc, { review: control.reviewTrack, disabled: busy });
      }
      review.textContent = state.mode === 'play' ? 'Review playback' : 'Review queue addition';
      for (const [field, element] of [['input', input], ['title', title]]) if (element.value !== state[field]) element.value = state[field];
      for (const element of [target, sourceType, input, title, mode]) element.disabled = state.writing;
      clear.disabled = state.writing;
      refresh.disabled = busy; review.disabled = busy || !state.target || (saved ? !state.playlist || state.playlistStatus !== 'ready' : !state.input.trim());
      message.textContent = state.message || (state.reading ? 'Reading the destination…' : '');
      result.textContent = state.result?.detail || '';
      if (shownReview !== state.pending) {
        shownReview = state.pending; consent.replaceChildren(); consent.hidden = !shownReview;
        if (shownReview) {
          const pending = shownReview;
          confirmButton = el('button', { type: 'button', text: modes[pending.mode] });
          const cancel = el('button', { type: 'button', text: 'Cancel' });
          confirmButton.addEventListener('click', () => { void control.confirm(pending); });
          cancel.addEventListener('click', () => control.cancel());
          const snapshot = pending.before.queue;
          const effect = pending.mode === 'play'
            ? pending.item.kind === 'artist' ? 'This replaces the group’s queue with up to 10 artist tracks and starts them in order.'
              : pending.item.kind === 'show' ? 'This replaces the group’s queue with up to 10 available episodes and starts them in Spotify’s order.'
              : pending.item.container ? 'This replaces the group’s queue and starts the album or playlist.'
              : 'This replaces the group’s current source and starts playback. The existing queue remains available.'
            : pending.mode === 'next' && snapshot.playing !== null
              ? `Add after queue position ${snapshot.playing}, keeping the existing queue and playback.`
              : `Append after the ${snapshot.total} existing queue entries. This does not start or resume playback.`;
          consent.append(el('p', { text: `${modes[pending.mode]}: ${pending.title || pending.item.uri} in ${pending.group.label}?` }),
            el('p', { text: `Current playback: ${pending.before.label}.` }),
            el('p', { text: effect }),
            ...(pending.item.kind === 'artist' ? [el('p', { class: 'source-help', text: 'Uses the artist’s top tracks or a small sample from their albums, in Spotify’s order. Requires Spotify sign-in.' })] : []),
            ...(pending.item.kind === 'show' ? [el('p', { class: 'source-help', text: 'Selects up to 10 available episodes from the show’s first episode page, in Spotify’s order. Requires Spotify sign-in.' })] : []),
            ...(snapshot && pending.mode !== 'play' && snapshot.playing === null ? [el('p', { text: 'The group is not using its queue. Select an existing queue entry separately when you are ready to start it.' })] : []),
            ...(pending.item.kind === 'sonos_playlist' ? [el('p', { class: 'source-help', text: 'The saved playlist is read again before sending. Its current contents are supplied by this household’s speakers.' })] : []),
            ...(pending.item.kind === 'sonos_playlist_track' ? [el('p', { class: 'source-help', text: `Track ${pending.item.position} from “${pending.item.playlistTitle}”. The exact saved occurrence is checked again before sending.` })] : []),
            el('p', { class: 'source-help', text: 'The destination is checked again before sending. Another controller can still change it afterward. The speakers must support and reach the source.' }),
            el('div', { class: 'row' }, confirmButton, cancel));
          consent.focus?.({ preventScroll: true });
          consent.scrollIntoView?.({ block: 'nearest' });
        }
      }
      if (confirmButton) confirmButton.disabled = busy || !state.pending;
    }
    async function refreshSources() {
      if (await control.refresh() && control.state.sourceType === 'playlist') await control.loadPlaylists();
    }
    target.addEventListener('change', () => {
      if (control.edit('target', target.value) && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    sourceType.addEventListener('change', () => {
      if (control.edit('sourceType', sourceType.value) && control.state.sourceType === 'playlist') void control.loadPlaylists();
    });
    playlist.addEventListener('change', () => control.edit('playlist', playlist.value));
    refreshPlaylists.addEventListener('click', () => { void control.loadPlaylists(); });
    readContents.addEventListener('click', () => { void control.loadContents(); });
    previousContents.addEventListener('click', () => { void control.loadContents('previous'); });
    nextContents.addEventListener('click', () => { void control.loadContents('next'); });
    input.addEventListener('input', () => control.edit('input', input.value));
    title.addEventListener('input', () => control.edit('title', title.value));
    mode.addEventListener('change', () => control.edit('mode', mode.value));
    refresh.addEventListener('click', () => { void refreshSources(); });
    review.addEventListener('click', () => { void control.review(); });
    clear.addEventListener('click', () => control.clearDraft());
    details.addEventListener('toggle', (event) => {
      if (event && event.target !== details) return;
      if (!details.open) control.close();
      else if (!control.state.open) void refreshSources();
    });
    const hide = () => { if (doc.hidden) control.close(); else if (details.open) void refreshSources(); };
    const win = doc.defaultView;
    const pagehide = () => control.close();
    const pageshow = () => { if (!doc.hidden && details.open) void refreshSources(); };
    doc.addEventListener('visibilitychange', hide);
    win?.addEventListener('pagehide', pagehide);
    win?.addEventListener('pageshow', pageshow);
    render(control.state);
    return { node, ...control, openSource(value) {
      const offered = control.offer(value);
      if (control.state.open) {
        details.open = true;
        target.focus({ preventScroll: true });
        node.scrollIntoView({ block: 'start' });
      }
      return offered;
    }, dispose() {
      doc.removeEventListener('visibilitychange', hide);
      win?.removeEventListener('pagehide', pagehide);
      win?.removeEventListener('pageshow', pageshow);
      control.dispose();
    } };
  }

  return { source, prepare, playlists, preparePlaylist, verifyPlaylist, contentsRequest, playlistContents, contentsSummary, renderContents,
    preparePlaylistTrack, trackQuery, verifyPlaylistTrack, trackOutcome,
    commandBody, offeredSource, groups, playback, queue, queued, outcome, model, panel, routes, modes, MAX_BYTES, MAX_TITLE };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SourcePlayerRemote;
else if (typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, SourcePlayerRemote.routes);
  const host = document.getElementById('households');
  if (host) {
    const sourcePanel = SourcePlayerRemote.panel({ call, el,
      async execute(group, work) {
        let parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
        if (!parts) {
          await loadZones();
          parts = [...house.cards.values()].find((card) => groupShape(card.zone) === group.signature);
        }
        if (!parts || !beginAction(parts)) throw new Error('The destination changed or another playback action is busy. Refresh rooms and review again.');
        try { return await work(); } finally { endAction(parts); }
      },
    });
    host.before(sourcePanel.node);
    document.addEventListener('fsonos:source-review', (event) => {
      event.preventDefault();
      void sourcePanel.openSource(event.detail);
    });
  }
}
