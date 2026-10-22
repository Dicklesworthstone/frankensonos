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

  const defaults = Object.freeze({ ...Object.fromEntries(Object.keys(lists).map((key) => [key, ''])),
    min_work_minutes: '', max_work_minutes: '', energy_bias: '0', allow_long_works: false, minutes: '' });
  const draft = () => Object.freeze({ ...defaults });
  function copyDraft(value) {
    if (!object(value) || Object.keys(value).some((key) => !Object.hasOwn(defaults, key))) {
      fail('The DJ direction draft is unreadable.');
    }
    const result = { ...defaults, ...value };
    for (const [key, raw] of Object.entries(result)) {
      if (key === 'allow_long_works' ? typeof raw !== 'boolean'
        : typeof raw !== 'string' || raw.length > (Object.hasOwn(lists, key) ? 16384 : 16)) {
        fail('The DJ direction draft contains an invalid field.');
      }
    }
    return result;
  }
  function editDraft(value, field, raw) {
    if (!Object.hasOwn(defaults, field)) fail('Unknown DJ direction field.');
    return Object.freeze(copyDraft({ ...copyDraft(value), [field]: raw }));
  }
  function fromDraft(value, mood = null) {
    const d = copyDraft(value), constraints = {};
    for (const key of Object.keys(lists)) {
      const items = d[key].split(/\r\n|\n|\r/).map((s) => s.trim()).filter(Boolean);
      constraints[key] = key.includes('decades') ? items.map((item) => {
        if (!/^\d{4,5}$/.test(item)) fail('Decades use their first year, such as 1960, one per line.');
        return Number(item);
      }) : items;
    }
    for (const key of ['min_work_minutes', 'max_work_minutes']) if (d[key] !== '') {
      if (!/^\d+$/.test(d[key])) fail('Work lengths must be whole minutes.');
      constraints[key] = Number(d[key]);
    }
    if (!/^-?[0-2]$/.test(d.energy_bias)) fail('Energy shift must be -2 to 2.');
    constraints.energy_bias = Number(d.energy_bias);
    constraints.allow_long_works = d.allow_long_works;
    const direction = { constraints };
    if (d.minutes !== '') {
      if (!/^\d+$/.test(d.minutes) || !integer(Number(d.minutes), 1, 10080)) {
        fail('DJ duration must be 1–10080 whole minutes, or blank for until cleared.');
      }
      direction.for_secs = Number(d.minutes) * 60;
    }
    return read(direction, mood);
  }

  // The caller owns draft/review lifecycle and restores focus after rerender.
  // This component only emits field edits; it cannot save or start music.
  function editor({ draft: value, change, disabled = false, el, idPrefix,
    focusAttribute = 'data-dj-direction-focus', open = false, toggle = () => {} }) {
    const d = copyDraft(value), helpId = `${idPrefix}-help`;
    const box = el('details', { class: 'dj-direction-editor', open,
      ontoggle: (event) => toggle(event.target.open) }, el('summary', { text: 'DJ filters and duration' }));
    const grid = el('div', { class: 'dj-direction-grid' });
    const attrs = (key, label) => ({ id: `${idPrefix}-${key}`, 'aria-label': label,
      [focusAttribute]: `${idPrefix}-${key}`, disabled });
    const field = (label, input) => el('label', { for: input.id }, label, input);
    const order = ['include_artists', 'exclude_artists', 'include_genres', 'exclude_genres',
      'decades', 'exclude_decades', 'include_keywords', 'exclude_keywords',
      'include_composers', 'exclude_composers', 'periods', 'exclude_periods'];
    for (const key of order) {
      const input = el('textarea', { ...attrs(key, lists[key]), rows: 2, maxlength: 16384,
        'aria-describedby': helpId, oninput: (event) => change(key, event.target.value),
        placeholder: key.includes('decades') ? '1960\n1970'
          : key.includes('periods') ? 'baroque\nlate_romantic' : 'One name per line' });
      input.value = d[key];
      grid.append(field(lists[key], input));
    }
    for (const [key, label, max] of [['min_work_minutes', 'Minimum work length (minutes)', 600],
      ['max_work_minutes', 'Maximum work length (minutes)', 600], ['minutes', 'Direction duration (minutes)', 10080]]) {
      grid.append(field(label, el('input', { ...attrs(key, label), type: 'number', min: 1, max, step: 1,
        inputmode: 'numeric', value: d[key], placeholder: key === 'minutes' ? 'Until cleared' : 'No limit',
        oninput: (event) => change(key, event.target.value) })));
    }
    const energy = el('select', { ...attrs('energy_bias', 'Energy shift'),
      onchange: (event) => change('energy_bias', event.target.value) });
    for (const [value, label] of [['-2', 'Much calmer'], ['-1', 'Calmer'], ['0', 'Keep program energy'],
      ['1', 'Brighter'], ['2', 'Much brighter']]) energy.append(el('option', { value, text: label }));
    energy.value = d.energy_bias;
    grid.append(field('Energy shift', energy));
    grid.append(el('label', { class: 'dj-direction-check' }, el('input', {
      ...attrs('allow_long_works', 'Allow long works'), type: 'checkbox', checked: d.allow_long_works,
      onchange: (event) => change('allow_long_works', event.target.checked) }), 'Allow long works'));
    box.append(el('p', { id: helpId, text: 'One entry per line; commas stay part of a name. Decades use the first year, such as 1960. Leave filters empty to use the selected mood or automatic program.' }),
      grid, el('p', { class: 'dj-direction-note', text: 'The duration starts afresh each time this scene or routine starts the DJ. With no mood or filters, it clears temporary steering and returns to the automatic program.' }));
    return box;
  }

  return Object.freeze({ lists: Object.freeze(lists), periods: Object.freeze(periods),
    constraintsOf, read, signature, describe, draft, editDraft, fromDraft, editor });
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DjDirection;
