// Shared wire checks for one ordered, bounded library selection.
'use strict';
const QueueTracksRemote = (() => {
  const MAX_TRACKS = 10;
  const uint = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const text = (value, max = 4096) => typeof value === 'string' && value.trim() !== '' && value.length <= max;
  const fail = message => { throw new Error(message); };
  const supported = source => typeof source === 'string' && /^spotify:(track|episode):[A-Za-z0-9]{22}$/.test(source);
  function tracksOf(value) {
    if (!Array.isArray(value) || !value.length || value.length > MAX_TRACKS) fail(`Select 1 to ${MAX_TRACKS} cached tracks.`);
    return Object.freeze(value.map(track => {
      if (!track || !supported(track.source_uri) || (track.title != null && typeof track.title !== 'string')) {
        fail('The selection contains an unsupported track. Refresh the listing.');
      }
      const title = track.title == null ? '' : [...track.title.trim()].slice(0, 256).join('');
      return Object.freeze({ source_uri: track.source_uri, ...(title ? { title } : {}) });
    }));
  }
  function queueOf(value, target) {
    const room = typeof target === 'string' ? target.slice(0, target.lastIndexOf('@')) : '';
    if (!value || !room || value.zone !== room || !uint(value.total) || !text(value.revision, 1024)
        || !Array.isArray(value.items) || value.items.length !== Math.min(1, value.total)
        || value.items.some(item => !item || item.position !== 1 || typeof item.title !== 'string')
        || (value.playing != null && (!uint(value.playing) || value.playing < 1 || value.playing > value.total))) {
      fail('The destination queue is unreadable. Refresh it before reviewing the selection.');
    }
    return Object.freeze({ revision: value.revision, total: value.total, playing: value.playing ?? null });
  }
  function verifyQueue(before, current, next) {
    if (!before || !current || before.revision !== current.revision || before.total !== current.total
        || (next && before.playing !== current.playing)) {
      fail('The destination queue changed. Review the selected tracks again.');
    }
  }
  // Phone selections retain source and group identity while their review is open.
  function playbackOf(value, group) {
    const zone = value?.zone, track = value?.track;
    if (!zone || !text(zone.household) || !text(zone.coordinator_room)
        || !Array.isArray(zone.members) || !zone.members.length || !zone.members.every(member => text(member))
        || !zone.members.includes(zone.coordinator_room) || new Set(zone.members).size !== zone.members.length
        || JSON.stringify([zone.household, zone.coordinator_room, [...zone.members].sort()]) !== group.signature
        || !['playing', 'paused', 'stopped', 'transitioning', 'unknown'].includes(value.transport_state)
        || (track != null && (!text(track.uri) || (track.queue_position != null
          && (!uint(track.queue_position) || track.queue_position === 0))))) {
      fail('Playback or group membership changed. Refresh before reviewing the selection.');
    }
    return JSON.stringify([group.signature, value.transport_state, track?.uri ?? null, track?.queue_position ?? null]);
  }
  function body(zone, tracks, next, queue) {
    if (!text(zone, 128) || typeof next !== 'boolean' || !text(queue?.revision, 1024)) fail('Review the destination queue again.');
    return Object.freeze({ zone, tracks: tracksOf(tracks), next, expected_revision: queue.revision });
  }
  function receipt(result, request, before) {
    const room = request.zone.slice(0, request.zone.lastIndexOf('@'));
    if (!result || !text(result.done) || result.zone !== room || !uint(result.first) || result.first < 1
        || result.first > before.total + 1 || result.count !== request.tracks.length
        || typeof result.playing_queue !== 'boolean'
        || (result.notes !== undefined && (!Array.isArray(result.notes)
          || result.notes.some(note => !note || typeof note.detail !== 'string')))) {
      fail('The queue selection acknowledgement was incomplete; its outcome is uncertain. Read the queue before another add.');
    }
    return { done: result.done, changed: true, notes: result.notes };
  }
  return { MAX_TRACKS, supported, tracksOf, queueOf, verifyQueue, playbackOf, body, receipt };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = QueueTracksRemote;
