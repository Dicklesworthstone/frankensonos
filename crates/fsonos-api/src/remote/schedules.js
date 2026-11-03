// Persisted routines run on the daemon, never on a browser timer.
'use strict';

const SchedulesRemote = (() => {
  const directionApi = () => typeof DjDirection !== 'undefined' ? DjDirection : require('./dj-direction.js');
  const queueApi = () => typeof SceneQueue !== 'undefined' ? SceneQueue : require('./scene-queue.js');
  const routes = {
    schedules: ['GET', '/schedules'],
    schedulePause: ['POST', '/schedules/pause'],
    scheduleResume: ['POST', '/schedules/resume'],
    scheduleRemove: ['POST', '/schedules/remove'],
    scheduleAdd: ['POST', '/schedules'],
    scheduleZones: ['GET', '/zones'],
    scheduleScenes: ['GET', '/scenes'],
    scheduleMoods: ['GET', '/dj/moods'],
  };
  const text = (v, max = 2048) => typeof v === 'string' && v.trim() !== '' && v.length <= max;
  const integer = (v, low, high) => Number.isSafeInteger(v) && v >= low && v <= high;
  const instant = (v) => text(v, 64) && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)$/.test(v)
    && Number.isFinite(Date.parse(v));
  const errorText = (e) => text(e?.message) ? `${e.message}${text(e.hint) ? ` ${e.hint}` : ''}` : 'The request failed.';
  const displayTime = (v) => v ? `${new Date(v).toLocaleString()} (your browser time; house reports ${v})` : 'None reported';

  function scheduleOf(value) {
    if (!value || !integer(value.id, 1, Number.MAX_SAFE_INTEGER) || !text(value.when, 256)
        || !text(value.summary, 128000) || !text(value.creator, 512) || typeof value.enabled !== 'boolean'
        || (value.next != null && !instant(value.next)) || (value.last_fired != null && !instant(value.last_fired))) {
      throw new Error('The schedule response is unreadable. Refresh before changing it.');
    }
    const fields = {
      dj_start: ['zone', 'mood', 'direction'], pause: ['zone', 'fade_secs'],
      volume: ['zone', 'volume'], apply_scene: ['scene'], announce: ['announcement'],
    };
    if (!Object.hasOwn(fields, value.action)) throw new Error('This schedule action is not supported by this remote.');
    for (const field of ['zone', 'mood', 'direction', 'scene', 'volume', 'fade_secs', 'announcement']) {
      if (!fields[value.action].includes(field) && value[field] != null) throw new Error('The schedule action has conflicting fields.');
    }
    if (value.action !== 'announce' && (value.action === 'apply_scene' ? !text(value.scene, 512) : !text(value.zone, 512))) {
      throw new Error('The schedule target is missing.');
    }
    if ((value.mood != null && !text(value.mood, 128))
        || (value.action === 'volume' && !integer(value.volume, 0, 100))
        || (value.fade_secs != null && !integer(value.fade_secs, 0, 600))) {
      throw new Error('The schedule settings are unreadable.');
    }
    return Object.freeze({ id: value.id, when: value.when, action: value.action,
      zone: value.zone ?? null, scene: value.scene ?? null, mood: value.mood ?? null,
      ...(value.direction != null ? { direction: directionApi().read(value.direction, value.mood) } : {}),
      ...(value.action === 'announce' ? { announcement: announcementOf(value.announcement) } : {}),
      volume: value.volume ?? null, fade_secs: value.fade_secs ?? null,
      summary: value.summary, creator: value.creator, enabled: value.enabled,
      next: value.next ?? null, last_fired: value.last_fired ?? null });
  }

  function schedulesOf(value) {
    if (!Array.isArray(value) || value.length > 10000) throw new Error('The schedule list is unreadable.');
    const seen = new Set();
    return value.map((v) => {
      const row = scheduleOf(v);
      if (seen.has(row.id)) throw new Error('The schedule list contains duplicate IDs.');
      seen.add(row.id);
      return row;
    });
  }

  // Next/last runs advance on their own; the definition and enabled state do not.
  function definition(row) {
    return JSON.stringify([row.id, row.when, row.action, row.zone, row.scene,
      row.mood, row.volume, row.fade_secs, row.summary, row.creator,
      row.direction == null ? null : directionApi().signature(row.direction, row.mood),
      announcementSignature(row.announcement)]);
  }

  const DAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
  const ACTIONS = { dj_start: 'Start the DJ', apply_scene: 'Apply a scene', volume: 'Set room volume', pause: 'Pause a group', announce: 'Make an announcement' };
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  const draftOf = (action) => ({ action, cadence: 'weekdays', time: '07:30', days: [],
    delay: '45m', at: '', zone: '', scene: '', mood: '', volume: '20', fade_secs: '0',
    ...(action === 'dj_start' ? { direction: directionApi().draft() } : {}),
    ...(action === 'announce' ? { announcement_source: 'text', announcement_text: '',
      announcement_chime: 'bell', announcement_voice: '', announcement_rooms: [], announcement_volume: '35' } : {}) });

  function stamp(value) {
    const canonical = (v, depth) => {
      if (depth > 16) throw new Error('The saved definition is too deeply nested.');
      if (Array.isArray(v)) return v.map((item) => canonical(item, depth + 1));
      return object(v) ? Object.fromEntries(Object.keys(v).sort().map((key) => [key, canonical(v[key], depth + 1)])) : v;
    };
    const result = JSON.stringify(canonical(value, 0));
    if (result.length > 128000) throw new Error('The saved definition is too large to review.');
    return result;
  }


  function announcementOf(value) {
    const fail = () => { throw new Error('The scheduled announcement needs text or a chime, valid rooms, and a volume from 0 to 100.'); };
    if (!object(value) || Object.keys(value).some((key) => !['text', 'chime', 'wav_base64', 'voice', 'rooms', 'volume'].includes(key))
        || value.wav_base64 != null || Number(value.text != null) + Number(value.chime != null) !== 1) fail();
    const recipe = {};
    if (value.text != null) {
      if (typeof value.text !== 'string' || !value.text.trim() || [...value.text.trim()].length > 1000) fail();
      recipe.text = value.text.trim();
      if (value.voice != null) {
        if (typeof value.voice !== 'string') fail();
        const voice = value.voice.trim();
        if (!voice || voice.startsWith('-') || /[\u0000-\u001f\u007f-\u009f]/u.test(voice)
            || new TextEncoder().encode(voice).length > 4096) fail();
        recipe.voice = voice;
      }
    } else {
      if (typeof value.chime !== 'string' || value.voice != null) fail();
      const chime = value.chime.trim().toLowerCase();
      if (!['bell', 'beep', 'rise'].includes(chime)) fail();
      recipe.chime = chime;
    }
    const rooms = value.rooms === undefined ? [] : value.rooms;
    if (!Array.isArray(rooms) || !rooms.every((room) => typeof room === 'string'
        && room.trim() && [...room.trim()].length <= 128)) fail();
    recipe.rooms = Object.freeze(rooms.map((room) => room.trim()));
    if (value.volume != null) {
      if (!integer(value.volume, 0, 100)) fail();
      recipe.volume = value.volume;
    }
    return Object.freeze(recipe);
  }

  function announcementSignature(value) {
    if (value == null) return null;
    const recipe = announcementOf(value);
    return JSON.stringify([recipe.text ?? null, recipe.chime ?? null, recipe.voice ?? null,
      [...recipe.rooms].sort(), recipe.volume ?? null]);
  }

  function announcementDescription(recipe) {
    const source = recipe.text != null ? `Say “${recipe.text}”${recipe.voice ? ` with voice ${recipe.voice}` : ''}`
      : `Play the ${recipe.chime} chime`;
    return `${source}; ${recipe.rooms.length ? recipe.rooms.join(', ') : 'every room'}; volume ${recipe.volume ?? 35} (house limits apply)`;
  }

  function announcementDestination(options, targets) {
    if (!Array.isArray(targets) || !targets.length || new Set(targets).size !== targets.length) {
      throw new Error('Choose at least one room for the announcement.');
    }
    const selected = targets.map((target) => options.rooms.find((room) => room.target === target));
    if (selected.some((room) => !room)) throw new Error('Choose announcement rooms from the current list.');
    return Object.freeze({ signature: stamp(selected.map((room) => [room.target, room.signature])),
      label: selected.map((room) => room.roomLabel).join(', ') });
  }

  function announcementDraft(draft) {
    if (!['text', 'chime'].includes(draft.announcement_source)
        || typeof draft.announcement_volume !== 'string' || !/^\d+$/.test(draft.announcement_volume)) {
      throw new Error('Choose speech or a chime and an announcement volume from 0 to 100.');
    }
    return announcementOf({
      ...(draft.announcement_source === 'text' ? { text: draft.announcement_text,
        ...(draft.announcement_voice ? { voice: draft.announcement_voice } : {}) } : { chime: draft.announcement_chime }),
      rooms: draft.announcement_rooms, volume: Number(draft.announcement_volume),
    });
  }

  function roomsOf(value) {
    if (!Array.isArray(value) || value.length > 512) throw new Error('The room list is unreadable.');
    const seen = new Set();
    return value.flatMap((group) => {
      if (!group || !text(group.household, 128) || !text(group.coordinator_room, 256)
          || !Array.isArray(group.members) || !group.members.length || group.members.length > 512
          || !group.members.every((r) => text(r, 256)) || !group.members.includes(group.coordinator_room)) {
        throw new Error('The group membership is incomplete.');
      }
      const signature = stamp([group.household, group.coordinator_room, [...group.members].sort()]);
      return group.members.map((name) => {
        const target = `${name}@${group.household}`;
        if (seen.has(target)) throw new Error('The room list is ambiguous.');
        seen.add(target);
        return Object.freeze({ target, signature, lead: name === group.coordinator_room,
          label: `${group.members.join(' + ')} (${group.household})`, roomLabel: `${name} (${group.household})` });
      });
    });
  }

  function scenesOf(value) {
    if (!Array.isArray(value) || value.length > 10000) throw new Error('The scene list is unreadable.');
    const seen = new Set();
    return value.map((v) => {
      if (!v || !text(v.name, 512) || !Number.isSafeInteger(v.saved) || !Array.isArray(v.groups)
          || !object(v.volumes) || !object(v.mutes) || seen.has(v.name)
          || !Object.values(v.volumes).every((n) => integer(n, 0, 100))
          || !Object.values(v.mutes).every((b) => typeof b === 'boolean')
          || !v.groups.every((g) => g && text(g.coordinator, 256) && Array.isArray(g.members)
            && g.members.every((r) => text(r, 256)) && object(g.source)
            && ['keep', 'favorite', 'uri', 'dj', 'queue'].includes(g.source.kind)
            && (g.source.kind === 'dj' || g.source.direction == null)
            && (g.source.kind === 'queue' || g.source.queue == null)
            && (g.playing == null || typeof g.playing === 'boolean'))) {
        throw new Error('A saved scene is incomplete or ambiguous.');
      }
      for (const group of v.groups) {
        if (group.source.kind === 'queue') queueApi().read(group.source.queue);
        if (group.source.direction != null) directionApi().read(group.source.direction, group.source.mood);
      }
      seen.add(v.name);
      return Object.freeze({ name: v.name, signature: stamp(v),
        details: JSON.stringify({ groups: v.groups, volumes: v.volumes, mutes: v.mutes }, null, 2) });
    });
  }

  function moodsOf(value) {
    if (!value || !Array.isArray(value.moods) || value.moods.length > 512) throw new Error('The DJ mood list is unreadable.');
    const seen = new Set();
    return value.moods.map((m) => {
      if (!m || !text(m.name, 128) || !object(m.constraints) || seen.has(m.name.trim().toLowerCase())) {
        throw new Error('A DJ mood is incomplete or ambiguous.');
      }
      seen.add(m.name.trim().toLowerCase());
      return Object.freeze({ name: m.name, signature: stamp(m) });
    });
  }

  async function choices(call, action) {
    if (action === 'apply_scene') return { scenes: scenesOf(await call('scheduleScenes')), rooms: [], moods: [] };
    const rooms = roomsOf(await call('scheduleZones'));
    const moods = action === 'dj_start' ? moodsOf(await call('scheduleMoods')) : [];
    return { rooms: ['volume', 'announce'].includes(action) ? rooms : rooms.filter((r) => r.lead), moods, scenes: [] };
  }

  function timeOf(draft) {
    if (draft.cadence === 'delay') {
      if (!text(draft.delay, 64)) throw new Error('Give a delay such as 45m, 1h30m or 2d.');
      const delay = draft.delay.replace(/\s/g, '').toLowerCase();
      if (!/^(?:\d+[dhms])+$/.test(delay)) throw new Error('Give a delay with d, h, m or s units.');
      let secs = 0;
      for (const [, n, unit] of delay.matchAll(/(\d+)([dhms])/g)) secs += Number(n) * { d: 86400, h: 3600, m: 60, s: 1 }[unit];
      if (!integer(secs, 1, 365 * 86400)) throw new Error('The delay must be positive and at most 365 days.');
      return Object.freeze({ when: `in ${delay}`, kind: 'delay' });
    }
    if (draft.cadence === 'once') {
      // Require whole seconds and an explicit offset, and reject normalized dates such as February 30.
      const at = typeof draft.at === 'string' ? draft.at.trim() : '';
      if (!/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:Z|[+-]\d\d:\d\d)$/.test(at) || !instant(at)) {
        throw new Error('Give an exact time with seconds and an offset, such as YYYY-MM-DDTHH:MM:SS-05:00.');
      }
      const [y, m, d, h, min, sec] = at.slice(0, 19).split(/[-T:]/).map(Number);
      const leap = y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0);
      const lengths = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
      if (y < 1 || m < 1 || m > 12 || d < 1 || d > lengths[m - 1] || h > 23 || min > 59 || sec > 59) {
        throw new Error('The exact date or time is invalid.');
      }
      return Object.freeze({ when: at, kind: 'once' });
    }
    if (!['daily', 'weekdays', 'weekends', 'custom'].includes(draft.cadence)
        || !/^(?:[01]\d|2[0-3]):[0-5]\d$/.test(draft.time)) throw new Error('Choose days and a house time in HH:MM format.');
    let days = draft.cadence;
    if (days === 'custom') {
      if (!Array.isArray(draft.days) || !draft.days.length || !draft.days.every((d) => DAYS.includes(d))) throw new Error('Choose at least one weekday.');
      days = DAYS.filter((d) => draft.days.includes(d)).join(',');
      days = { 'mon,tue,wed,thu,fri,sat,sun': 'daily', 'mon,tue,wed,thu,fri': 'weekdays', 'sat,sun': 'weekends' }[days] || days;
    }
    return Object.freeze({ when: `${days} ${draft.time}`, kind: 'weekly' });
  }

  function creation(draft, options) {
    if (!draft || !Object.hasOwn(ACTIONS, draft.action) || !options) throw new Error('Choose an action and refresh its available targets.');
    const timing = timeOf(draft);
    const body = { when: timing.when, action: draft.action };
    const destination = draft.action === 'apply_scene' ? options.scenes.find((s) => s.name === draft.scene)
      : draft.action === 'announce' ? announcementDestination(options, draft.announcement_rooms)
        : options.rooms.find((r) => r.target === draft.zone);
    if (!destination) throw new Error('Choose an explicit destination from the current list.');
    let mood = null;
    if (draft.action === 'apply_scene') body.scene = destination.name;
    else if (draft.action === 'announce') body.announcement = announcementDraft(draft);
    else body.zone = destination.target;
    if (draft.action === 'dj_start' && draft.mood) {
      mood = options.moods.find((m) => m.name === draft.mood);
      if (!mood) throw new Error('Choose a current DJ mood or the automatic program.');
      body.mood = mood.name.trim().toLowerCase();
    }
    if (draft.action === 'dj_start') body.direction = directionApi().fromDraft(draft.direction, body.mood);
    for (const [action, field, max] of [['volume', 'volume', 100], ['pause', 'fade_secs', 600]]) {
      if (draft.action !== action) continue;
      if (typeof draft[field] !== 'string' || !/^\d+$/.test(draft[field]) || !integer(Number(draft[field]), 0, max)) {
        throw new Error(`${field === 'volume' ? 'Volume' : 'Fade seconds'} must be a whole number from 0 to ${max}.`);
      }
      body[field] = Number(draft[field]);
    }
    const label = draft.action === 'apply_scene' ? `scene “${destination.name}”`
      : draft.action === 'announce' ? announcementDescription(body.announcement)
      : draft.action === 'volume' ? `${destination.roomLabel} to ${body.volume}`
        : draft.action === 'dj_start' ? `${destination.label}; ${directionApi().describe(body.direction, body.mood)}`
          : `${destination.label}, fade ${body.fade_secs}s`;
    return Object.freeze({ action: 'create', body: Object.freeze(body), timing, destination, mood,
      summary: `${ACTIONS[draft.action]}: ${label}` });
  }

  function sameAction(row, body) {
    return row.action === body.action && row.zone === (body.zone ?? null) && row.scene === (body.scene ?? null)
      && row.mood === (body.mood ?? null)
      && directionApi().signature(row.direction, row.mood) === directionApi().signature(body.direction, body.mood)
      && row.volume === (body.volume ?? null)
      && announcementSignature(row.announcement) === announcementSignature(body.announcement)
      && (row.action !== 'pause' || (row.fade_secs ?? 0) === (body.fade_secs ?? 0));
  }

  function sameWhen(row, planned) {
    if (planned.timing.kind === 'weekly') return row.when === planned.body.when;
    if (!row.when.startsWith('once ') || !instant(row.when.slice(5))) return false;
    return planned.timing.kind === 'delay' || Date.parse(row.when.slice(5)) === Date.parse(planned.body.when);
  }

  function noDuplicate(rows, planned) {
    // Relative delays acquire an absolute deadline on the daemon, so they cannot be compared here.
    if (planned.timing.kind === 'delay') return;
    const duplicate = rows.find((r) => sameAction(r, planned.body) && sameWhen(r, planned));
    if (duplicate) throw new Error(`An equivalent schedule #${duplicate.id} already exists. Manage that routine instead.`);
  }

  function model({ call, changed = () => {} }) {
    let epoch = 0;
    let disposed = false;
    const state = { open: false, rows: [], loaded: false, loading: false,
      writing: false, pending: null, message: '', draft: null, options: null };
    const publish = () => {
      // A view failure must never turn an acknowledged write into a retry.
      if (!disposed) { try { changed(state); } catch (_) { /* The data model remains authoritative. */ } }
    };
    const current = (version) => !disposed && state.open && epoch === version;
    const invalidate = () => { ++epoch; state.pending = null; state.loaded = false; state.loading = false; state.draft = null; state.options = null; };

    async function refresh() {
      if (disposed || state.writing) return false;
      state.open = true;
      invalidate();
      const version = epoch;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const rows = schedulesOf(await call('schedules'));
        if (!current(version)) return false;
        state.rows = rows;
        state.loaded = true;
        return true;
      } catch (e) {
        if (current(version)) { state.rows = []; state.message = errorText(e); }
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    async function beginCreate(action = 'dj_start') {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || !Object.hasOwn(ACTIONS, action)) return false;
      const version = ++epoch;
      state.pending = null;
      state.draft = draftOf(action);
      state.options = null;
      state.loading = true;
      state.message = '';
      publish();
      try {
        const options = await choices(call, action);
        if (!current(version)) return false;
        state.options = options;
        return true;
      } catch (e) {
        if (current(version)) state.message = errorText(e);
        return false;
      } finally {
        if (current(version)) { state.loading = false; publish(); }
      }
    }

    function edit(field, value) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing || !state.draft
          || field === 'action' || field === 'direction' || !Object.hasOwn(state.draft, field)
          || (field === 'days' ? !Array.isArray(value) || !value.every((v) => DAYS.includes(v))
            : field === 'announcement_rooms' ? !Array.isArray(value) || !value.every((v) => typeof v === 'string' && [...v].length <= 128)
              : typeof value !== 'string' || value.length > (field === 'announcement_text' ? 2000 : field === 'announcement_voice' ? 4096 : 512))) return false;
      state.draft = { ...state.draft, [field]: Array.isArray(value) ? [...value] : value };
      state.pending = null;
      state.message = '';
      publish();
      return true;
    }

    function editDirection(field, value) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing
          || state.draft?.action !== 'dj_start') return false;
      state.pending = null;
      try {
        const direction = directionApi().editDraft(state.draft.direction, field, value);
        state.draft = { ...state.draft, direction };
        state.message = '';
        publish();
        return true;
      } catch (e) {
        state.message = errorText(e);
        publish();
        return false;
      }
    }

    function reviewCreate() {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing) return false;
      try {
        const planned = creation(state.draft, state.options);
        noDuplicate(state.rows, planned);
        state.pending = planned;
        state.message = '';
        publish();
        return true;
      } catch (e) {
        state.pending = null;
        state.message = errorText(e);
        publish();
        return false;
      }
    }

    function dismissCreate() {
      if (disposed || state.writing) return;
      ++epoch;
      state.draft = null;
      state.options = null;
      state.pending = null;
      state.loading = false;
      publish();
    }

    function review(action, id) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing) return false;
      const row = state.rows.find((r) => r.id === id);
      if (!row || !['pause', 'resume', 'remove'].includes(action)
          || (action === 'pause' && !row.enabled) || (action === 'resume' && row.enabled)) return false;
      state.draft = null;
      state.options = null;
      state.pending = Object.freeze({ action, row });
      publish();
      return true;
    }

    function cancel() {
      if (disposed || state.writing) return;
      ++epoch;
      state.pending = null;
      publish();
    }

    async function confirm(reviewed) {
      if (disposed || !state.open || !state.loaded || state.loading || state.writing
          || !reviewed || reviewed !== state.pending) return false;
      const version = ++epoch;
      state.pending = null;
      state.writing = true;
      state.message = '';
      publish();
      let sent = false;
      try {
        const rows = schedulesOf(await call('schedules'));
        if (!current(version)) return false;
        if (reviewed.action === 'create') {
          noDuplicate(rows, reviewed);
          const options = await choices(call, reviewed.body.action);
          if (!current(version)) return false;
          const destination = reviewed.body.action === 'apply_scene'
            ? options.scenes.find((s) => s.name === reviewed.body.scene)
            : reviewed.body.action === 'announce' ? announcementDestination(options, reviewed.body.announcement.rooms)
              : options.rooms.find((r) => r.target === reviewed.body.zone);
          const mood = reviewed.mood && options.moods.find((m) => m.name === reviewed.mood.name);
          if (!destination || destination.signature !== reviewed.destination.signature
              || (reviewed.mood && mood?.signature !== reviewed.mood.signature)) {
            throw new Error('The target group, scene or mood changed. Refresh and review it again.');
          }
          sent = true;
          const result = scheduleOf(await call('scheduleAdd', { body: reviewed.body }));
          if (rows.some((r) => r.id === result.id) || !result.enabled || !sameAction(result, reviewed.body)
              || !sameWhen(result, reviewed) || !result.next
              || (reviewed.timing.kind !== 'weekly' && Date.parse(result.next) !== Date.parse(result.when.slice(5)))) {
            throw new Error('The reply did not confirm the reviewed new routine.');
          }
          if (current(version)) {
            state.rows = [...rows, result];
            state.message = `Created schedule #${result.id}: ${result.when}. Next: ${displayTime(result.next)}. Refresh before another change.`;
          }
          return true;
        }
        const row = rows.find((r) => r.id === reviewed.row.id);
        if (!row || definition(row) !== definition(reviewed.row) || row.enabled !== reviewed.row.enabled) {
          throw new Error('The schedule changed or was removed. Refresh and review it again.');
        }
        const route = { pause: 'schedulePause', resume: 'scheduleResume', remove: 'scheduleRemove' }[reviewed.action];
        sent = true;
        const result = scheduleOf(await call(route, { body: { id: row.id } }));
        const expected = reviewed.action === 'remove' ? row.enabled : reviewed.action === 'resume';
        if (definition(result) !== definition(row) || result.enabled !== expected) {
          throw new Error('The reply did not confirm the reviewed schedule change.');
        }
        if (current(version)) {
          state.message = `Schedule #${row.id} ${reviewed.action === 'remove' ? 'removed' : reviewed.action === 'pause' ? 'paused' : 'resumed'}. Refresh to see its current state.`;
        }
        return true;
      } catch (e) {
        if (current(version)) state.message = `${errorText(e)}${sent ? ' The outcome may be uncertain; the request was not repeated.' : ''}${sent && reviewed.action === 'create' ? ' It may already be saved. Inspect the schedule list before creating another routine.' : ''} Refresh before another change.`;
        return false;
      } finally {
        state.writing = false;
        state.loaded = false;
        state.draft = null;
        state.options = null;
        publish();
      }
    }

    return { state, refresh, review, cancel, confirm, beginCreate, edit, editDirection, reviewCreate, dismissCreate,
      close() { state.open = false; invalidate(); publish(); },
      dispose() { disposed = true; ++epoch; } };
  }

  function panel({ call, el }) {
    const content = el('div', { class: 'schedules-body' });
    const node = el('details', { class: 'schedules-remote scenes' }, el('summary', { text: 'Scheduled routines' }), content);
    const control = model({ call, changed: render });
    const button = (label, key, work, disabled) => el('button', { type: 'button', text: label,
      'data-schedule-focus': key, disabled, onclick: work });
    let reviewFocus = null;
    let directionOpen = false;
    function creationForm(s) {
      const d = s.draft;
      const busy = s.loading || s.writing;
      const form = el('section', { class: 'schedule-form', 'aria-label': 'New routine' },
        el('h3', { text: 'Create a routine' }));
      const field = (label, key, attrs = {}) => el('label', {}, label,
        el('input', { type: 'text', value: d[key], 'aria-label': label, 'data-schedule-focus': key,
          disabled: busy, oninput: (e) => control.edit(key, e.target.value), ...attrs }));
      const select = (label, key, items, change = (v) => control.edit(key, v)) => {
        const input = el('select', { 'aria-label': label, 'data-schedule-focus': key, disabled: busy,
          onchange: (e) => change(e.target.value) });
        for (const [value, name] of items) input.append(el('option', { value, text: name }));
        input.value = d[key];
        return el('label', {}, label, input);
      };
      form.append(select('Routine action', 'action', Object.entries(ACTIONS), (v) => control.beginCreate(v)),
        select('When', 'cadence', [['daily', 'Every day'], ['weekdays', 'Weekdays'], ['weekends', 'Weekends'],
          ['custom', 'Choose weekdays'], ['delay', 'Once after a delay'], ['once', 'Once at an exact time']]));
      if (d.cadence === 'custom') {
        const days = el('fieldset', { class: 'schedule-days', disabled: busy }, el('legend', { text: 'Days in the house’s time zone' }));
        for (const day of DAYS) days.append(el('label', {}, el('input', { type: 'checkbox', checked: d.days.includes(day),
          'aria-label': day, 'data-schedule-focus': `day-${day}`, onchange: (e) => control.edit('days',
            e.target.checked ? [...d.days, day] : d.days.filter((v) => v !== day)) }), day));
        form.append(days);
      }
      if (d.cadence === 'delay') form.append(field('Delay from daemon acceptance (maximum 365 days)', 'delay', { maxlength: 64 }),
        el('p', { text: 'Use units: 45m, 1h30m, 90s or 2d. The daemon computes the deadline when it accepts the request, not when this review opens.' }));
      else if (d.cadence === 'once') form.append(field('Exact time with seconds and offset', 'at', { maxlength: 64,
        placeholder: 'YYYY-MM-DDTHH:MM:SS-05:00' }), el('p', { text: 'Include Z for UTC or an explicit offset. The daemon rejects times already past; no browser time-zone conversion is applied.' }));
      else form.append(field('House time', 'time', { type: 'time', step: 60 }),
        el('p', { text: 'Uses the daemon host’s local time zone. The daemon handles daylight-saving transitions and reports the next run after saving.' }));
      const options = s.options;
      if (d.action === 'apply_scene') form.append(select('Scene', 'scene', [['', 'Choose a saved scene…'],
        ...(options?.scenes || []).map((v) => [v.name, v.name])]),
        el('p', { text: 'This schedules the scene by name, not a frozen copy. Later edits to that scene change what future runs do.' }));
      else if (d.action === 'announce') {
        form.append(select('Announcement sound', 'announcement_source', [['text', 'Speak a message'], ['chime', 'Play a chime']]));
        if (d.announcement_source === 'text') form.append(
          el('label', {}, 'Message (maximum 1000 characters)', el('textarea', { value: d.announcement_text, maxlength: 2000,
            rows: 3, 'aria-label': 'Announcement message', 'data-schedule-focus': 'announcement_text', disabled: busy,
            oninput: (e) => control.edit('announcement_text', e.target.value) })),
          field('Voice (optional; uses the configured voice when blank)', 'announcement_voice', { maxlength: 4096 }));
        else form.append(select('Chime', 'announcement_chime', [['bell', 'Bell'], ['beep', 'Beep'], ['rise', 'Rise']]));
        const targets = el('fieldset', { class: 'schedule-days', disabled: busy }, el('legend', { text: 'Announcement rooms' }));
        for (const room of options?.rooms || []) targets.append(el('label', {},
          el('input', { type: 'checkbox', checked: d.announcement_rooms.includes(room.target), disabled: busy,
            'aria-label': room.roomLabel, 'data-schedule-focus': `announcement-room-${room.target}`,
            onchange: (e) => control.edit('announcement_rooms', e.target.checked
              ? [...d.announcement_rooms, room.target] : d.announcement_rooms.filter((target) => target !== room.target)) }), room.roomLabel));
        form.append(targets, field('Announcement volume (0–100)', 'announcement_volume', { type: 'number', min: 0, max: 100, step: 1 }),
          el('p', { text: 'Each run announces in the selected rooms and restores their music. House volume limits apply when it runs. Speech uses the daemon host’s configured local speech engine.' }));
      }
      else form.append(select(d.action === 'volume' ? 'Room' : 'Group', 'zone', [['', 'Choose a destination…'],
        ...(options?.rooms || []).map((v) => [v.target, d.action === 'volume' ? v.roomLabel : v.label])]),
        el('p', { text: d.action === 'volume' ? 'Changes only the selected room’s volume, subject to house limits.'
          : 'Acts on the selected room’s group at run time. Regrouping later can change which rooms hear it.' }));
      if (d.action === 'dj_start') form.append(select('DJ mood', 'mood', [['', 'Automatic time-of-day program'],
        ...(options?.moods || []).map((v) => [v.name, v.name])]),
        directionApi().editor({ draft: d.direction, change: (field, value) => control.editDirection(field, value),
          disabled: busy, el, idPrefix: 'schedule-direction', focusAttribute: 'data-schedule-focus',
          open: directionOpen, toggle: (open) => { directionOpen = open; } }),
        el('p', { text: 'Each run replaces the group’s DJ steering before selecting new music. Already queued works remain.' }));
      if (d.action === 'volume') form.append(field('Volume (0–100)', 'volume', { type: 'number', min: 0, max: 100, step: 1 }));
      if (d.action === 'pause') form.append(field('Fade seconds (0–600; 0 pauses immediately)', 'fade_secs', { type: 'number', min: 0, max: 600, step: 1 }));
      form.append(el('div', { class: 'row' },
        button('Cancel new routine', 'dismiss-create', () => control.dismissCreate(), s.writing),
        button('Reload form', 'reload-create', () => control.beginCreate(d.action), busy),
        button('Review new routine', 'review-create', () => control.reviewCreate(), busy || !options)));
      return form;
    }


    function render(s) {
      const active = document.activeElement;
      const focus = content.contains(active) ? active.getAttribute('data-schedule-focus') : null;
      const caret = active?.selectionStart;
      if (s.pending && focus && focus !== 'cancel') reviewFocus = focus;
      content.replaceChildren();
      content.setAttribute('aria-busy', String(s.loading || s.writing));
      content.append(el('p', { text: 'Routines are stored by the daemon and run with their creator’s rights. Daily and weekly times use the house’s time zone, not this browser’s.' }),
        el('div', { class: 'row' }, button('Refresh schedules', 'refresh', () => control.refresh(), s.loading || s.writing),
          button('New routine', 'new-create', () => control.beginCreate(), !s.loaded || s.loading || s.writing || !!s.draft || !!s.pending)));
      if (s.draft) content.append(creationForm(s));
      if (s.message) content.append(el('p', { role: 'status', text: s.message }));
      if (s.loading) content.append(el('p', { role: 'status', text: 'Reading schedules…' }));
      if (s.writing) content.append(el('p', { role: 'status', text: 'Checking and saving the reviewed change. Do not repeat it from another tab.' }));
      if (s.pending?.action === 'create') {
        const p = s.pending;
        const review = el('section', { class: 'schedule-review', 'aria-label': 'Review new routine' },
          el('h3', { text: 'Create this enabled routine?' }),
          el('p', { text: p.body.when }), el('p', { text: p.summary }),
          el('p', { text: 'It runs on the daemon, even after you close this page, with your caller’s permissions and house safety limits. Runs missed by more than ten minutes are skipped.' }),
          el('p', { text: 'The target, mood or scene will be checked again before sending. That check is not an atomic lock; future regrouping or edits may change the routine’s effects.' }));
        if (p.destination.details) review.append(el('details', {}, el('summary', { text: 'Reviewed scene definition' }),
          el('pre', { text: p.destination.details })));
        review.append(el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel(), false),
          button('Create routine', 'confirm', () => control.confirm(p), s.writing)));
        content.append(review);
      } else if (s.pending) {
        const p = s.pending;
        content.append(el('section', { class: 'schedule-review', 'aria-label': 'Review schedule change' },
          el('h3', { text: `${p.action === 'remove' ? 'Remove' : p.action === 'pause' ? 'Pause' : 'Resume'} schedule #${p.row.id}?` }),
          el('p', { text: `${p.row.when}: ${p.row.summary}` }),
          ...(p.row.announcement ? [el('p', { text: announcementDescription(p.row.announcement) })] : []),
          el('p', { text: p.action === 'remove' ? 'This permanently removes the saved routine, not the music or scene. A run already in progress is not cancelled. Speaker undo does not restore schedules.'
            : p.action === 'pause' ? 'Prevent future runs; this does not pause music or cancel a run already in progress.'
              : 'Enable future runs. Missed runs are not made up; the daemon decides the next eligible time.' }),
          el('p', { text: 'The saved definition is checked again before sending. A different client can still change it between that check and the write.' }),
          el('div', { class: 'row' }, button('Cancel', 'cancel', () => control.cancel(), false),
            button('Confirm schedule change', 'confirm', () => control.confirm(p), s.writing))));
      }
      if (s.loaded && !s.rows.length) content.append(el('p', { text: 'No saved schedules.' }));
      for (const row of s.rows) {
        const busy = !s.loaded || s.loading || s.writing || !!s.pending;
        content.append(el('article', { class: 'schedule-card' },
          el('h3', { text: `#${row.id} · ${row.enabled ? row.next ? 'Enabled' : 'Enabled; no next run' : 'Paused'}` }),
          el('p', { text: row.when }), el('p', { text: row.summary }),
          ...(row.announcement ? [el('p', { text: announcementDescription(row.announcement) })] : []),
          el('p', { text: `Creator: ${row.creator}` }),
          el('p', { text: `Next: ${displayTime(row.next)}` }),
          el('p', { text: `Last run or skipped occurrence: ${displayTime(row.last_fired)}` }),
          el('div', { class: 'row' },
            button(row.enabled ? 'Pause routine' : 'Resume routine', `toggle-${row.id}`,
              () => control.review(row.enabled ? 'pause' : 'resume', row.id), busy),
            button('Remove routine', `remove-${row.id}`, () => control.review('remove', row.id), busy))));
      }
      const key = s.pending ? 'cancel' : focus === 'cancel' ? reviewFocus : focus === 'confirm' ? 'refresh' : focus;
      const next = [...content.querySelectorAll('[data-schedule-focus]')].find((e) => e.getAttribute('data-schedule-focus') === key);
      if (next && !next.disabled) {
        next.focus({ preventScroll: true });
        if ((next.type === 'text' || next.tagName === 'TEXTAREA') && Number.isInteger(caret)) next.setSelectionRange(caret, caret);
      }
    }
    node.addEventListener('toggle', () => { if (node.open) control.refresh(); else control.close(); });
    render(control.state);
    return { node, dispose: control.dispose };
  }

  return { routes, scheduleOf, schedulesOf, definition, announcementOf, roomsOf, scenesOf, moodsOf, timeOf, creation, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SchedulesRemote;
else {
  Object.assign(ROUTES, SchedulesRemote.routes);
  document.getElementById('scenes-section')?.before(SchedulesRemote.panel({ call, el }).node);
}
