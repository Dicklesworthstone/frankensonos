// Shared decoding and explanation of saved queue sources and authored selections.
'use strict';
const SceneQueue = (() => {
  const modes = ['NORMAL', 'REPEAT_ALL', 'REPEAT_ONE', 'SHUFFLE_NOREPEAT', 'SHUFFLE', 'SHUFFLE_REPEAT_ONE'];
  const uint = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const identity = value => typeof value === 'string' && value.trim() !== '' && value.length <= 1024
    && !/[\x00-\x1f\x7f]/.test(value);
  const MAX_TRACKS = 10;

  function readTracks(value) {
    if (!Array.isArray(value) || !value.length || value.length > MAX_TRACKS) {
      throw new Error(`A scene selection needs 1 to ${MAX_TRACKS} Spotify tracks or episodes.`);
    }
    return Object.freeze(value.map(track => {
      if (!track || typeof track !== 'object' || Array.isArray(track)
          || Object.keys(track).some(key => !['source_uri', 'title'].includes(key))
          || typeof track.source_uri !== 'string' || !/^spotify:(track|episode):[A-Za-z0-9]{22}$/.test(track.source_uri)
          || (track.title != null && (typeof track.title !== 'string' || !track.title.trim()
            || [...track.title].length > 256 || /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/u.test(track.title)))) {
        throw new Error('Each scene selection entry needs a complete Spotify track or episode URI and an optional title of 1 to 256 characters.');
      }
      return Object.freeze({ source_uri: track.source_uri, ...(track.title != null ? { title: track.title } : {}) });
    }));
  }

  const tracksSignature = value => readTracks(value).map(track => [track.source_uri, track.title ?? null]);
  function describeTracks(value) {
    const tracks = readTracks(value);
    return `Ordered Spotify selection (${tracks.length} ${tracks.length === 1 ? 'entry' : 'entries'}; replaces the queue on recall):\n`
      + tracks.map((track, index) => `${index + 1}. ${track.title ? track.title + ' · ' : ''}${track.source_uri}`).join('\n');
  }

  function read(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)
        || !identity(value.coordinator) || (value.household != null && !identity(value.household))
        || !uint(value.update_id) || !uint(value.track) || value.track < 1 || !uint(value.position_secs)
        || (value.play_mode != null && !modes.includes(value.play_mode))
        || (value.boot_seq != null && !uint(value.boot_seq))
        || (value.total != null && (!uint(value.total) || value.total < 1
          || value.total > 1000 || value.track > value.total))) {
      throw new Error('The scene’s saved queue reference is unreadable.');
    }
    return Object.freeze({ coordinator: value.coordinator, household: value.household ?? null,
      update_id: value.update_id, track: value.track, position_secs: value.position_secs,
      play_mode: value.play_mode ?? null, boot_seq: value.boot_seq ?? null,
      ...(value.total != null ? { total: value.total } : {}) });
  }

  function signature(value) {
    const q = read(value);
    return [q.coordinator, q.household, q.update_id, q.track, q.position_secs, q.play_mode, q.boot_seq, q.total ?? null];
  }

  function describe(value) {
    const q = read(value), position = `${Math.floor(q.position_secs / 60)}:${String(q.position_secs % 60).padStart(2, '0')}`;
    if (q.total != null) return `Saved queue backup, ${q.total} entries, track #${q.track} at ${position}${q.play_mode ? ` · ${q.play_mode.replaceAll('_', ' ')}` : ''}. Recall replaces the original player’s current queue. Bounded queue undo is available only after a complete verified scene on unchanged groups; inspect the result and undo preview.`;
    return `Saved queue, track #${q.track} at ${position}${q.play_mode ? ` · ${q.play_mode.replaceAll('_', ' ')}` : ''}. Requires the original player and unchanged queue. Queue contents are not saved.`;
  }

  return { read, signature, describe, MAX_TRACKS, readTracks, tracksSignature, describeTracks };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = SceneQueue;
