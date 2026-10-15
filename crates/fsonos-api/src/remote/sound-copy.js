// Copy observed speaker tone through individually logged native EQ commands.
'use strict';
const SoundCopyRemote = (() => {
  const routes = { soundCopyRead: ['GET', '/zones/{room}/eq'], soundCopyWrite: ['POST', '/eq'] };
  const FIELDS = Object.freeze(['bass', 'treble', 'loudness']);
  const MAX_TARGETS = 32, READ_CONCURRENCY = 4;
  const text = (v) => typeof v === 'string' && v.trim() !== '' && v.length <= 4096;
  const level = (v) => Number.isInteger(v) && v >= -10 && v <= 10;
  const sound = (v) => Object.freeze(Object.fromEntries(FIELDS.map((f) => [f, v[f]])));
  const same = (a, b) => FIELDS.every((f) => a[f] === b[f]);
  const problem = (e) => [typeof e?.message === 'string' ? e.message : 'The request failed.',
    typeof e?.hint === 'string' ? e.hint : ''].filter(Boolean).join(' ');

  function inventory(value) {
    if (!Array.isArray(value) || value.length > 1024) throw new Error('The room list is unreadable. Refresh rooms.');
    const found = [], seen = new Set();
    for (const group of value) {
      if (!group || !text(group.household) || !text(group.coordinator_room)
          || !Array.isArray(group.members) || !group.members.length || group.members.length > 1024
          || !group.members.every(text) || !group.members.includes(group.coordinator_room)) {
        throw new Error('The room list is incomplete. Refresh rooms.');
      }
      for (const room of group.members) {
        const target = `${room}@${group.household}`;
        if (seen.has(target)) throw new Error('The room list has ambiguous names. Refresh rooms.');
        if (found.length >= 1024) throw new Error('The room list is too large.');
        seen.add(target);
        found.push(Object.freeze({ target, room, household: group.household, label: `${room} (${group.household})` }));
      }
    }
    return Object.freeze(found);
  }
  function observed(value, room) {
    if (!value || value.room !== room.room || !level(value.bass) || !level(value.treble)
        || typeof value.loudness !== 'boolean') {
      throw new Error(`The sound reported for ${room.label} is incomplete or belongs to another room.`);
    }
    return sound(value);
  }
  function requireRooms(fresh, wanted) {
    for (const room of wanted) {
      if (!fresh.some((r) => r.target === room.target && r.room === room.room && r.household === room.household)) {
        throw new Error(`${room.label} is no longer available. Refresh rooms.`);
      }
    }
  }

  function model({ call, changed = () => {} }) {
    let rooms = Object.freeze([]), source = '', targets = Object.freeze([]), sourceTone = null;
    let review = null, report = null, detail = 'Choose a source and destinations; nothing is selected automatically.';
    let open = false, active = true, disposed = false, reading = false, writing = false;
    let fresh = false, stopping = false, epoch = 0;
    const live = () => !disposed && active && open;
    const idle = () => live() && !reading && !writing;
    const chosen = () => rooms.find((r) => r.target === source);
    const ready = () => Boolean(idle() && fresh && sourceTone && chosen() && targets.length);
    const view = () => Object.freeze({ rooms, source, targets, sourceTone, review, report, detail,
      open, reading, writing, stopping, fresh: fresh && live(), canPrepare: ready(), active: live() });
    const emit = () => { if (!disposed) { try { changed(view()); } catch (_) { /* A view cannot change a command's outcome. */ } } };
    function invalidate(why) { ++epoch; reading = false; fresh = false; review = null; detail = why; emit(); }
    const read = async (room) => observed(await call('soundCopyRead', { params: { room: room.target } }), room);
    async function readMany(wanted, valid) {
      let next = 0, failure = null;
      const result = new Array(wanted.length);
      await Promise.all(Array.from({ length: Math.min(READ_CONCURRENCY, wanted.length) }, async () => {
        while (valid() && !failure && next < wanted.length) {
          const index = next++, room = wanted[index];
          try { result[index] = await read(room); }
          catch (error) { failure = new Error(`${room.label}: ${problem(error)}`); }
        }
      }));
      if (failure) throw failure;
      return result;
    }
    async function refresh() {
      if (disposed) return false;
      open = true;
      if (!idle()) { emit(); return false; }
      const version = ++epoch, valid = () => live() && epoch === version;
      reading = true; fresh = false; review = null; sourceTone = null; detail = 'Reading available speakers…'; emit();
      try {
        const found = inventory(await call('zones'));
        if (!valid()) return false;
        rooms = found;
        if (!rooms.some((r) => r.target === source)) source = '';
        targets = Object.freeze(targets.filter((t) => t !== source && rooms.some((r) => r.target === t)));
        detail = rooms.length ? 'Choose the speaker whose current sound to copy.' : 'No speakers found. Refresh after discovery.';
        return true;
      } catch (error) {
        if (valid()) { rooms = Object.freeze([]); targets = Object.freeze([]); source = ''; detail = problem(error); }
        return false;
      } finally { if (valid()) { reading = false; emit(); } }
    }
    function selectSource(target) {
      if (!live() || writing || !rooms.some((r) => r.target === target)) return false;
      source = target; sourceTone = null; targets = Object.freeze(targets.filter((t) => t !== source));
      invalidate('Read the source speaker’s current sound.'); return true;
    }
    async function readSource() {
      const room = chosen();
      if (!idle() || !room) return false;
      const version = ++epoch, valid = () => live() && epoch === version && source === room.target;
      reading = true; fresh = false; review = null; detail = 'Reading source sound…'; emit();
      try {
        const tone = await read(room);
        if (!valid()) return false;
        sourceTone = tone; fresh = true; detail = 'Source sound observed. Select the speakers to receive it.';
        return true;
      } catch (error) { if (valid()) detail = problem(error); return false; }
      finally { if (valid()) { reading = false; emit(); } }
    }
    function selectTarget(target, selected) {
      if (!idle() || typeof selected !== 'boolean' || target === source || !rooms.some((r) => r.target === target)) return false;
      if (selected && !targets.includes(target) && targets.length >= MAX_TARGETS) {
        detail = `Choose at most ${MAX_TARGETS} destinations per copy.`; emit(); return false;
      }
      targets = Object.freeze(selected ? [...new Set([...targets, target])] : targets.filter((t) => t !== target));
      ++epoch; review = null; emit(); return true;
    }
    async function prepare() {
      if (!ready()) return null;
      const from = chosen(), destinations = targets.map((t) => rooms.find((r) => r.target === t));
      const version = ++epoch, valid = () => live() && epoch === version;
      reading = true; review = null; detail = 'Reading source and destination sound for the review…'; emit();
      try {
        requireRooms(inventory(await call('zones')), [from, ...destinations]);
        if (!valid()) return null;
        const tones = await readMany([from, ...destinations], valid);
        if (!valid()) return null;
        sourceTone = tones[0]; fresh = true;
        review = Object.freeze({ from, tone: sourceTone, entries: Object.freeze(destinations.map((room, i) =>
          Object.freeze({ room, before: tones[i + 1], patch: Object.freeze(Object.fromEntries(
            FIELDS.filter((f) => tones[i + 1][f] !== sourceTone[f]).map((f) => [f, sourceTone[f]]))) }))) });
        detail = 'Review every destination. The source will not be written.';
        return review;
      } catch (error) { if (valid()) { fresh = false; detail = problem(error); } return null; }
      finally { if (valid()) { reading = false; emit(); } }
    }
    function row(index, status, detail, tone = null) {
      report = Object.freeze({ ...report, rows: Object.freeze(report.rows.map((r, i) => i === index
        ? Object.freeze({ ...r, status, detail, observed: tone }) : r)) });
      emit();
    }
    async function confirm(selected) {
      if (!selected || selected !== review || !ready()) return false;
      const version = ++epoch, valid = () => live() && epoch === version && !stopping;
      review = null; fresh = false; writing = true; stopping = false;
      report = Object.freeze({ from: selected.from, tone: selected.tone, rows: Object.freeze(selected.entries.map((e) =>
        Object.freeze({ room: e.room, status: 'pending', detail: 'Not sent.', observed: null }))) });
      detail = 'Checking all speakers before sending any changes…'; emit();
      let complete = false, reason = '';
      try {
        const wanted = [selected.from, ...selected.entries.map((e) => e.room)];
        requireRooms(inventory(await call('zones')), wanted);
        if (!valid()) return false;
        const tones = await readMany(wanted, valid);
        if (!valid()) return false;
        if (!same(tones[0], selected.tone)) throw new Error('The source sound changed. Read it and review a new copy.');
        for (let i = 0; i < selected.entries.length; ++i) {
          if (!same(tones[i + 1], selected.entries[i].before) && !same(tones[i + 1], selected.tone)) {
            row(i, 'conflict', 'Sound changed after the review. No copy commands were sent.', tones[i + 1]);
            throw new Error(`${selected.entries[i].room.label} changed. Review all destinations again.`);
          }
        }
        for (let i = 0; i < selected.entries.length && valid(); ++i) {
          const entry = selected.entries[i];
          let sent = false, acknowledged = false;
          try {
            // Recheck each destination just before its turn, not only at batch start.
            const before = await read(entry.room);
            if (!valid()) break;
            if (same(before, selected.tone)) { row(i, 'unchanged', 'Already matches; no command sent.', before); continue; }
            if (!same(before, entry.before)) {
              reason = `${entry.room.label} changed before its turn.`;
              row(i, 'conflict', 'Sound changed before this speaker’s turn. Remaining copies stopped.', before); break;
            }
            if (!valid()) break;
            row(i, 'sending', 'Sending once…');
            if (!valid()) { row(i, 'not_sent', 'Stopped before dispatch.'); break; }
            sent = true;
            const reply = await call('soundCopyWrite', { body: { zone: entry.room.target, ...entry.patch } });
            if (!text(reply?.done) || !same(observed(reply?.eq, entry.room), selected.tone)) {
              throw new Error('The reply did not confirm the reviewed settings.');
            }
            acknowledged = true;
            row(i, 'accepted_unverified', 'Command accepted; the speaker’s sound has not been verified.');
            if (!valid()) break;
            const actual = await read(entry.room);
            if (!valid()) break;
            if (!same(actual, selected.tone)) {
              reason = `${entry.room.label} reported different sound after the command.`;
              row(i, 'different', 'The speaker reports different sound. Remaining copies stopped; nothing was repeated.', actual); break;
            }
            row(i, 'verified', 'Copied and independently read back from the speaker.', actual);
          } catch (error) {
            reason = `${entry.room.label}: ${problem(error)}`;
            row(i, acknowledged ? 'accepted_unverified' : sent ? 'uncertain' : 'not_sent',
              `${problem(error)} Remaining copies stopped; no command was repeated.`);
            break;
          }
        }
        complete = valid() && report.rows.every((r) => ['verified', 'unchanged'].includes(r.status));
      } catch (error) { reason = problem(error); }
      finally {
        for (let i = 0; i < report.rows.length; ++i) {
          if (report.rows[i].status === 'pending') row(i, 'not_attempted', 'Not sent because this copy stopped.');
        }
        writing = false;
        const verified = report.rows.filter((r) => r.status === 'verified').length;
        const unchanged = report.rows.filter((r) => r.status === 'unchanged').length;
        detail = complete ? `Copy complete: ${verified} verified, ${unchanged} already matched. The source speaker was not written.`
          : `Copy stopped. ${verified} verified; check each speaker’s result below. Already sent commands cannot be canceled. ${reason}`;
        emit();
      }
      return complete;
    }
    function cancel() {
      ++epoch; review = null; reading = false; fresh = false;
      if (writing) stopping = true;
      detail = writing ? 'Stopping after any command already in flight. Remaining speakers will not be changed.' : 'Copy review canceled. Read source sound before a new review.';
      emit();
    }
    return { view, refresh, selectSource, readSource, selectTarget, prepare, confirm, cancel,
      close() { open = false; cancel(); },
      suspend() { active = false; cancel(); },
      resume() { active = true; invalidate('Read source sound and review again after returning to this page.'); },
      dispose() { disposed = true; open = false; cancel(); },
    };
  }

  return { model, routes, inventory, observed, MAX_TARGETS, READ_CONCURRENCY };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = SoundCopyRemote;
