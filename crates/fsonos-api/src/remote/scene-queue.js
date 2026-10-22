// Shared decoding and explanation of a captured, unchanged speaker queue.
'use strict';
const SceneQueue = (() => {
  const modes = ['NORMAL', 'REPEAT_ALL', 'REPEAT_ONE', 'SHUFFLE_NOREPEAT', 'SHUFFLE', 'SHUFFLE_REPEAT_ONE'];
  const uint = value => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const identity = value => typeof value === 'string' && value.trim() !== '' && value.length <= 1024
    && !/[\x00-\x1f\x7f]/.test(value);

  function read(value) {
    if (!value || typeof value !== 'object' || Array.isArray(value)
        || !identity(value.coordinator) || (value.household != null && !identity(value.household))
        || !uint(value.update_id) || !uint(value.track) || value.track < 1 || !uint(value.position_secs)
        || (value.play_mode != null && !modes.includes(value.play_mode))
        || (value.boot_seq != null && !uint(value.boot_seq))) {
      throw new Error('The scene’s saved queue reference is unreadable.');
    }
    return Object.freeze({ coordinator: value.coordinator, household: value.household ?? null,
      update_id: value.update_id, track: value.track, position_secs: value.position_secs,
      play_mode: value.play_mode ?? null, boot_seq: value.boot_seq ?? null });
  }

  function signature(value) {
    const q = read(value);
    return [q.coordinator, q.household, q.update_id, q.track, q.position_secs, q.play_mode, q.boot_seq];
  }

  function describe(value) {
    const q = read(value), position = `${Math.floor(q.position_secs / 60)}:${String(q.position_secs % 60).padStart(2, '0')}`;
    return `Saved queue, track #${q.track} at ${position}${q.play_mode ? ` · ${q.play_mode.replaceAll('_', ' ')}` : ''}. Requires the original player and unchanged queue. Queue contents are not saved.`;
  }

  return { read, signature, describe };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = SceneQueue;
