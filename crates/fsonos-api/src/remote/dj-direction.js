// Reusable DJ selection rules shared by live steering, scenes and routines.
'use strict';

const DjDirection = (() => {
  const lists = {
    include_composers: 'Only composers', exclude_composers: 'Exclude composers',
    include_artists: 'Only artists', exclude_artists: 'Exclude artists',
    include_genres: 'Only genres', exclude_genres: 'Exclude genres',
    periods: 'Only classical periods', exclude_periods: 'Exclude classical periods',
    decades: 'Only release decades', exclude_decades: 'Exclude release decades',
    include_keywords: 'Include keywords', exclude_keywords: 'Exclude keywords',
  };
  const periods = ['medieval', 'renaissance', 'baroque', 'classical', 'romantic',
    'late_romantic', 'impressionist', 'modern', 'contemporary'];
  const text = (v) => typeof v === 'string' && v.trim() !== '';
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const fail = (message) => { throw new Error(message); };
  const word = (v) => text(v) && [...v.trim()].length <= 128;

  // Canonical, sparse wire constraints. Missing serde-default fields are empty,
  // but unknown fields and wrong types must never be silently dropped on edit.
  function constraintsOf(value = {}) {
    if (!object(value)) fail('Constraints must be an object.');
    const allowed = [...Object.keys(lists), 'min_work_minutes', 'max_work_minutes', 'energy_bias', 'allow_long_works'];
    if (Object.keys(value).some((key) => !allowed.includes(key))) fail('Unknown DJ constraint. Refresh or use the current API.');
    const result = {};
    for (const key of Object.keys(lists)) {
      const items = value[key] === undefined ? [] : value[key];
      if (!Array.isArray(items) || items.length > 32) fail(`${lists[key]} takes at most 32 entries.`);
      const cleaned = items.map((item) => {
        if (key === 'decades' || key === 'exclude_decades') {
          if (!integer(item, 1000, 65535) || item % 10 !== 0) fail('Decades use their first year, such as 1960.');
          return item;
        }
        if (!word(item)) fail(`${lists[key]} entries need 1–128 characters.`);
        const s = item.trim();
        if (key === 'periods' || key === 'exclude_periods') {
          const period = s.toLowerCase().replace(/[- ]/g, '_');
          if (!periods.includes(period)) fail(`Unknown classical period: ${s}.`);
          return period;
        }
        return s;
      });
      if (cleaned.length) result[key] = Object.freeze(cleaned);
    }
    for (const key of ['min_work_minutes', 'max_work_minutes']) {
      if (value[key] !== undefined && value[key] !== null) {
        if (!integer(value[key], 1, 600)) fail('Work lengths must be whole minutes from 1 to 600.');
        result[key] = value[key];
      }
    }
    if (result.min_work_minutes > result.max_work_minutes) fail('Minimum work length exceeds maximum.');
    if (value.energy_bias !== undefined && !integer(value.energy_bias, -2, 2)) fail('Energy shift must be -2 to 2.');
    if (value.energy_bias) result.energy_bias = value.energy_bias;
    if (value.allow_long_works !== undefined && typeof value.allow_long_works !== 'boolean') fail('Long-work permission must be a boolean.');
    if (value.allow_long_works) result.allow_long_works = true;
    return Object.freeze(result);
  }


  function read(value, mood = null) {
    if (!object(value) || Object.keys(value).some((key) => !['constraints', 'for_secs'].includes(key))) {
      fail('The saved DJ direction is unreadable.');
    }
    if (mood != null && !word(mood)) fail('The saved DJ mood is unreadable.');
    const constraints = constraintsOf(value.constraints);
    const result = { constraints };
    if (value.for_secs != null) {
      if (!integer(value.for_secs, 1, 604800)) fail('A saved DJ duration must be 1–604800 seconds.');
      if (!mood && !Object.keys(constraints).length) fail('A DJ duration needs a mood or a constraint.');
      result.for_secs = value.for_secs;
    }
    return Object.freeze(result);
  }

  // Absence retains legacy steering; an explicit empty object clears it.
  const signature = (value, mood = null) => value == null ? null : JSON.stringify(read(value, mood));
  function describe(value, mood = null) {
    const direction = read(value, mood), c = direction.constraints;
    if (!mood && !Object.keys(c).length) return 'DJ · automatic program (clears current steering)';
    const parts = mood ? [`DJ · ${mood}`] : ['DJ'];
    for (const [key, label] of Object.entries(lists)) if (c[key]?.length) parts.push(`${label}: ${c[key].join(', ')}`);
    if (c.min_work_minutes != null) parts.push(`minimum work length ${c.min_work_minutes} minutes`);
    if (c.max_work_minutes != null) parts.push(`maximum work length ${c.max_work_minutes} minutes`);
    if (c.energy_bias) parts.push(`energy ${c.energy_bias > 0 ? '+' : ''}${c.energy_bias}`);
    if (c.allow_long_works) parts.push('long works allowed');
    if (direction.for_secs != null) {
      const secs = direction.for_secs;
      const [n, unit] = secs % 3600 === 0 ? [secs / 3600, 'hour']
        : secs % 60 === 0 ? [secs / 60, 'minute'] : [secs, 'second'];
      parts.push(`for ${n} ${unit}${n === 1 ? '' : 's'} on each start`);
    }
    return parts.join('; ');
  }

  return Object.freeze({ lists: Object.freeze(lists), periods: Object.freeze(periods),
    constraintsOf, read, signature, describe });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjDirection;
