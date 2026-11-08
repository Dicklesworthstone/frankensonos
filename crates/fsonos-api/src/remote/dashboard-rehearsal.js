// The dashboard's read-only DJ preview uses the remote's existing planner contract.
'use strict';
const DashboardRehearsal = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const R = typeof DjSessionRemote !== 'undefined' ? DjSessionRemote : require('./dj-session.js');
  const lists = [
    ['include_artists', 'Only artists'], ['exclude_artists', 'Exclude artists'],
    ['include_genres', 'Only genres'], ['exclude_genres', 'Exclude genres'],
    ['include_composers', 'Only composers'], ['exclude_composers', 'Exclude composers'],
    ['include_keywords', 'Include keywords'], ['exclude_keywords', 'Exclude keywords'],
  ];
  // Keep the house's date and offset. Converting through Date would silently
  // change the requested house time to the browser's timezone.
  const houseTime = value => value.replace('T', ' ').replace(/(Z|[+-]\d\d:\d\d)$/, ' $1');

  function model({ call, controller, changed = () => {}, now = Date.now, demo = false }) {
    let active = true, disposed = false;
    const engine = R.rehearsalModel({ now, changed, call: async (operation, args = {}) => {
      if (operation === 'sessionZones') return call('/zones');
      if (operation === 'sessionMoods') return call('/dj/moods');
      if (operation === 'sessionRehearse') {
        try { return await call('/dj/rehearse', args.body); }
        catch (error) {
          if (error?.status) throw error;
          // The generic dashboard transport treats a missing POST receipt as
          // a potentially applied command. This particular POST only plans;
          // use the shared rehearsal model's read-only failure explanation.
          throw Object.assign(new Error('The rehearsal response was not received.', { cause: error }), { uncertain: true });
        }
      }
      throw new Error('Unknown rehearsal observation.');
    } });
    const available = () => !!(!disposed && active && controller.state.active
      && controller.state.topologyFresh && controller.selected()?.fresh);
    const canGenerate = () => !!(!demo && available() && engine.state.open && !engine.state.busy
      && engine.state.zone?.target === controller.state.selected
      && engine.state.zone?.signature === C.shape(controller.selected().zone)
      && !controller.state.busy && !controller.state.loading);
    function observe() {
      if (disposed) return;
      // Playback events briefly mark this observation stale while it is
      // refreshed. A rehearsal remains a valid historical snapshot through
      // those events. Retire it only when the selected group actually changes
      // or the page is suspended; fresh reads still gate another generation.
      const item = active && controller.state.active ? controller.selected() : null;
      engine.context(item ? R.zonesOf([item.zone])[0] : null);
      changed(engine.state);
    }
    function close() { engine.close(); }
    return { state: engine.state, available, canGenerate, observe,
      open() {
        observe();
        if (!available()) return false;
        engine.open();
        return true;
      },
      close,
      loadMoods() { return available() && engine.state.open ? engine.loadMoods() : Promise.resolve(false); },
      edit() { if (active && !disposed && engine.state.open) engine.edit(); },
      generate(draft) { return canGenerate() ? engine.generate(draft) : Promise.resolve(false); },
      handoff() { return canGenerate() ? engine.handoff() : null; },
      suspend() { active = false; close(); engine.context(null); },
      resume() { if (!disposed) { active = true; observe(); } },
      dispose() { disposed = true; active = false; engine.dispose(); } };
  }

  function panel({ call, controller, document: doc = document, now, demo = false, onedit = null }) {
    const $ = id => doc.getElementById(id), dialog = $('rehearsal-dialog'), trigger = $('open-rehearsal');
    let control, rendered = null, renderedMoods = null, visibleWorks = 24, transferProblem = '';
    const el = (tag, cls, text) => {
      const node = doc.createElement(tag);
      if (cls) node.className = cls;
      if (text != null) node.textContent = text;
      return node;
    };
    const button = (text, cls = 'outline-button') => { const node = el('button', cls, text); node.type = 'button'; return node; };
    const field = (name, label, node) => {
      node.id = 'rehearsal-' + name;
      const row = el('label', 'rehearsal-field', label); row.htmlFor = node.id; row.append(node); return row;
    };
    const input = (type, value = '') => { const node = el('input'); node.type = type; node.value = value; return node; };
    const option = (value, label) => { const node = el('option', '', label); node.value = value; return node; };
    const header = el('header'), close = button('Close', 'text-button'); close.id = 'close-rehearsal';
    close.setAttribute('aria-label', 'Close DJ rehearsal');
    header.append(el('span', 'eyebrow', 'A LOOK AT THE MUSIC AHEAD'), close);
    const heading = el('h2', '', 'Rehearse a DJ set'); heading.id = 'rehearsal-title';
    const context = el('p', 'review-target'); context.id = 'rehearsal-target';
    const introduction = el('p', 'small-note', 'Plan from your library, listening history and preferences. This preview does not start music, edit the queue or save steering.');
    const fields = el('fieldset', 'rehearsal-fieldset'); fields.append(el('legend', 'sr-only', 'Rehearsal settings'));
    const form = el('form', 'rehearsal-form'); form.id = 'rehearsal-form';
    const mode = el('select'); mode.append(option('works', 'Number of works'), option('hours', 'Hours of music')); mode.value = 'works';
    const length = input('number', '12'); length.min = '1'; length.max = '500'; length.required = true;
    const at = input('text'); at.placeholder = 'Now, or 23:00'; at.maxLength = 64; at.autocomplete = 'off';
    const seed = input('number', '1'); seed.min = '0'; seed.max = String(Number.MAX_SAFE_INTEGER); seed.required = true;
    const mood = el('select'); mood.append(option('', 'Current steering and time-of-day program'));
    const settings = el('div', 'rehearsal-settings');
    settings.append(field('mode', 'Plan by', mode), field('length', 'Length', length),
      field('at', 'Start in house time', at), field('seed', 'Seed', seed), field('mood', 'Mood for this preview', mood));
    fields.append(settings, el('p', 'small-note', 'Blank start means now. HH:MM uses the house’s clock; an RFC 3339 date with an offset is also accepted. The same seed repeats the set while its start time, library, history and settings stay the same.'));
    const filters = el('details', 'rehearsal-filters'), filterGrid = el('div', 'rehearsal-filter-grid');
    filters.append(el('summary', '', 'Temporary filters'), el('p', 'small-note', 'A named mood or any nonempty filter replaces this group’s current steering for the preview. Standing taste still applies. Names use one entry per line; commas remain part of the name.'));
    const filterInputs = Object.fromEntries(lists.map(([key, label]) => {
      const node = el('textarea'); node.rows = 2; node.maxLength = 32768; node.placeholder = 'One entry per line';
      filterGrid.append(field(key, label, node)); return [key, node];
    }));
    const minimum = input('number'), maximum = input('number');
    for (const node of [minimum, maximum]) { node.min = '1'; node.max = '600'; }
    const energy = el('select');
    for (const number of [-2, -1, 0, 1, 2]) energy.append(option(String(number), number === 0 ? 'Neutral' : `${number > 0 ? '+' : ''}${number} · ${number > 0 ? 'more energetic' : 'calmer'}`));
    energy.value = '0';
    const long = input('checkbox'), strict = input('checkbox');
    filterGrid.append(field('minimum', 'Minimum work minutes', minimum), field('maximum', 'Maximum work minutes', maximum),
      field('energy', 'Energy shift', energy), field('long', 'Allow long works', long), field('strict', 'Keep selection filters strict', strict));
    const clear = button('Clear temporary filters', 'text-button'); clear.id = 'rehearsal-clear';
    filters.append(filterGrid, el('p', 'small-note', 'Strict filters stay in force when matching music is scarce, so the set may be empty. A strict mood remains strict even when this box is off.'), clear);
    const actions = el('div', 'rehearsal-actions'), refresh = button('Refresh moods'); refresh.id = 'rehearsal-moods';
    const generate = button('Generate preview', 'primary-button'); generate.type = 'submit'; generate.id = 'rehearsal-generate';
    actions.append(refresh, generate); fields.append(filters, actions); form.append(fields);
    const moodsMessage = el('p', 'small-note'); moodsMessage.id = 'rehearsal-mood-message';
    const message = el('p', 'rehearsal-message'); message.id = 'rehearsal-message'; message.setAttribute('role', 'status'); message.setAttribute('aria-live', 'polite');
    const output = el('section', 'rehearsal-output'); output.id = 'rehearsal-output'; output.setAttribute('aria-label', 'Rehearsal result');
    const snapshot = el('p', 'rehearsal-snapshot'); snapshot.id = 'rehearsal-snapshot';
    const transfer = button('Review settings for live DJ', 'outline-button'); transfer.id = 'rehearsal-transfer';
    const transferHint = el('p', 'small-note'); transferHint.id = 'rehearsal-transfer-hint';
    transfer.setAttribute('aria-describedby', transferHint.id);
    const transferBox = el('section'); transferBox.id = 'rehearsal-transfer-box'; transferBox.append(transfer, transferHint);
    const result = el('div'); result.id = 'rehearsal-result'; output.append(snapshot, transferBox, result);
    dialog.replaceChildren(header, heading, context, introduction, form, moodsMessage, message, output);

    const draft = () => ({ lengthMode: mode.value, length: length.value, at: at.value, seed: seed.value, mood: mood.value,
      filters: { ...Object.fromEntries(lists.map(([key]) => [key, filterInputs[key].value])),
        min_work_minutes: minimum.value, max_work_minutes: maximum.value, energy_bias: energy.value,
        allow_long_works: long.checked, strict: strict.checked } });

    function workNode(row, index) {
      const work = row.work, item = el('li', 'rehearsal-work'), stamp = el('div', 'rehearsal-work-time');
      stamp.append(el('span', 'rehearsal-index', String(index + 1)), el('time', '', houseTime(row.at)));
      stamp.children[1].dateTime = row.at;
      const content = el('article', 'rehearsal-work-content');
      content.append(el('h4', '', work.title), el('p', 'rehearsal-artist', work.composer),
        el('p', 'small-note', [...(work.performers || []), work.album, work.minutes == null ? null : `${work.minutes} min`,
          work.movements > 1 ? `${work.movements} movements, kept together` : null].filter(Boolean).join(' · ')));
      content.append(el('p', 'rehearsal-why', work.reason.summary));
      const tags = el('p', 'rehearsal-tags');
      for (const text of [row.genre, row.mood ? `Mood: ${row.mood}` : null,
        `Energy ${row.energy}${row.energy_target == null ? '' : ` · target ${row.energy_target}`}`,
        row.explicit ? 'Explicit content' : null].filter(Boolean)) tags.append(el('span', '', text));
      content.append(tags);
      if (work.reason.relaxed?.length) content.append(el('p', 'rehearsal-warning', `Filters relaxed for this pick: ${work.reason.relaxed.join('; ')}.`));
      if (work.reason.factors?.length) {
        const details = el('details', 'rehearsal-factors'), factors = el('ul');
        details.append(el('summary', '', 'Selection factors'));
        for (const factor of work.reason.factors) factors.append(el('li', '', `${factor.factor}: ×${(factor.weight / 1000).toLocaleString(undefined, { maximumFractionDigits: 3 })}`));
        details.append(el('p', 'small-note', 'Multipliers from the DJ’s actual selection. ×1 is neutral.'), factors); content.append(details);
      }
      item.append(stamp, content); return item;
    }

    function hourly(hours) {
      const section = el('section', 'rehearsal-hours'); section.append(el('h3', '', 'Energy and variety through the set'),
        el('p', 'small-note', 'Each row groups works starting in that house-clock hour. Energy is their mean on a 0–100 scale; the target is the first pick’s target. Variety is Shannon entropy in bits: 0 means one artist or genre, higher values mean a more varied mix.'));
      const wrap = el('div', 'rehearsal-table-wrap'), table = el('table'), head = el('thead'), tr = el('tr');
      for (const title of ['First work in hour', 'Works', 'Energy', 'Artist variety', 'Genre variety']) { const th = el('th', '', title); th.scope = 'col'; tr.append(th); }
      head.append(tr); table.append(head); const body = el('tbody');
      for (const hour of hours) {
        const row = el('tr'), time = el('th', '', houseTime(hour.from)); time.scope = 'row';
        const energyCell = el('td', 'rehearsal-energy'), meter = el('meter'); meter.min = 0; meter.max = 100; meter.value = hour.energy;
        meter.setAttribute('aria-label', `Mean energy ${hour.energy} out of 100 for works starting ${houseTime(hour.from)}`);
        energyCell.append(meter, el('span', '', `${hour.energy} / 100`), el('small', '', hour.energy_target == null ? 'No target' : `Target ${hour.energy_target}`));
        row.append(time, el('td', '', String(hour.works)), energyCell,
          el('td', '', `${hour.artist_entropy.toFixed(2)} bits`), el('td', '', `${hour.genre_entropy.toFixed(2)} bits`)); body.append(row);
      }
      table.append(body); wrap.append(table); section.append(wrap); return section;
    }

    function renderReport(state) {
      result.replaceChildren(); visibleWorks = 24;
      const report = state.report, sum = report.summary, request = state.request;
      const stats = el('dl', 'rehearsal-stats');
      for (const [label, value] of [['Works', sum.works], ['Minutes', sum.minutes], ['Artists', sum.distinct_artists], ['Explicit works', sum.explicit]]) {
        const cell = el('div'); cell.append(el('dt', '', label), el('dd', '', String(value))); stats.append(cell);
      }
      result.append(stats, el('p', 'small-note', `From ${houseTime(report.from)} · seed ${report.seed} · requested ${request.works ?? request.hours} ${request.works == null ? 'hours (up to 500 works)' : 'works'}. ${request.mood ? `Mood: ${request.mood}.` : request.constraints ? 'Temporary filters replace live steering.' : 'Uses current steering and time-of-day programs.'}`));
      if (request.hours) result.append(el('p', 'small-note', 'The preview includes whole works that start within the requested window, so the last work may finish later.'));
      const applied = R.constraintsText(request.constraints);
      if (applied.length) {
        const details = el('details', 'rehearsal-applied'), items = el('ul'); details.append(el('summary', '', 'Captured temporary filters'));
        for (const text of applied) items.append(el('li', '', text)); details.append(items); result.append(details);
      }
      if (!sum.works) result.append(el('p', 'rehearsal-empty', request.constraints?.strict
        ? 'No works match these strict filters. Adjust the filters or sync more music; the preview will not select outside them.'
        : 'No works matched this preview. Try another mood or sync your library.'));
      if (request.works && sum.works > 0 && sum.works < request.works) result.append(el('p', 'rehearsal-warning', 'Fewer works were available than requested. This is the returned set, not a full-length playlist.'));
      if (request.hours && sum.works === 500) result.append(el('p', 'rehearsal-warning', 'The 500-work limit was reached. The requested window may not be fully covered.'));
      if (sum.hours.length) result.append(hourly(sum.hours));
      if (!sum.works) return;
      const timeline = el('section', 'rehearsal-timeline'), works = el('ol', 'rehearsal-work-list'), more = button('Show more works'); more.id = 'rehearsal-more';
      timeline.append(el('h3', '', 'The planned sequence'), el('p', 'small-note', 'Whole works stay together. Each start time includes the house’s date and UTC offset, including sets that cross midnight.'), works, more);
      function show() {
        const from = works.children.length, to = Math.min(visibleWorks, report.works.length);
        for (let index = from; index < to; index++) works.append(workNode(report.works[index], index));
        more.hidden = to === report.works.length; more.textContent = `Show ${Math.min(24, report.works.length - to)} more works (${to} of ${report.works.length} shown)`;
      }
      more.addEventListener('click', () => { visibleWorks += 24; show(); }); show(); result.append(timeline);
    }

    function render(state) {
      trigger.disabled = !control.available(); trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) dialog.showModal();
      else if (!state.open && dialog.open) { dialog.close(); if (!trigger.disabled) trigger.focus(); }
      context.textContent = state.zone?.label || 'Refresh playback and choose a current listening group.';
      fields.disabled = !state.open || !state.zone || state.busy;
      generate.disabled = !control.canGenerate(); refresh.disabled = !control.available() || state.loadingMoods || state.busy;
      form.setAttribute('aria-busy', String(state.busy)); generate.textContent = state.busy ? 'Planning…' : 'Generate preview';
      if (renderedMoods !== state.moods) {
        renderedMoods = state.moods; const chosen = mood.value;
        mood.replaceChildren(option('', 'Current steering and time-of-day program'));
        for (const name of state.moods) mood.append(option(name, name));
        if (chosen && !state.moods.includes(chosen)) { const missing = option(chosen, `${chosen} · unavailable, choose another mood`); missing.disabled = true; mood.append(missing); }
        mood.value = chosen;
      }
      moodsMessage.textContent = state.loadingMoods ? 'Reading available moods…' : state.moodError;
      message.textContent = demo ? 'Connect to your running house to rehearse from its library.'
        : transferProblem || (state.busy ? 'Planning from the current library and history. Playback continues as it is.' : state.message);
      output.hidden = !state.report;
      transferBox.hidden = typeof onedit !== 'function' || !state.report;
      transfer.disabled = !control.handoff();
      transferHint.textContent = state.edited ? 'Generate the edited settings again before copying them to live direction.'
        : !state.report?.works.length ? 'An empty rehearsal has no tested choices to review.'
          : !state.request?.mood && !Object.keys(state.request?.constraints || {}).length
            ? 'This preview inherited current steering. Use Advanced direction to edit it; no temporary settings were created.'
            : !control.canGenerate() ? 'Refresh playback for this unchanged listening group before copying its settings.'
              : 'Opens an unsaved direction draft for this group. Choose its lifetime, then review saving or starting the DJ. It does not copy the exact playlist, seed, start time, length or relaxed filters.';
      snapshot.textContent = state.edited
        ? 'Settings changed. This result still uses the captured settings below. Generate again to compare.'
        : 'Snapshot of the library, history and preferences when generated. Future picks can change as you listen or edit settings.';
      snapshot.className = 'rehearsal-snapshot' + (state.edited ? ' is-edited' : '');
      if (rendered === state.report) return;
      rendered = state.report;
      if (state.report) renderReport(state); else result.replaceChildren();
    }

    control = model({ call, controller, now, demo, changed: render });
    trigger.addEventListener('click', () => { if (control.open()) void control.loadMoods(); });
    close.addEventListener('click', control.close);
    dialog.addEventListener('cancel', event => { event.preventDefault(); control.close(); });
    dialog.addEventListener('close', () => { if (control.state.open) control.close(); });
    form.addEventListener('submit', event => { event.preventDefault(); transferProblem = ''; void control.generate(draft()); });
    form.addEventListener('input', control.edit); form.addEventListener('change', control.edit);
    mode.addEventListener('change', () => { length.max = mode.value === 'hours' ? '48' : '500'; length.value = mode.value === 'hours' ? '2' : '12'; control.edit(); });
    refresh.addEventListener('click', () => { void control.loadMoods(); });
    clear.addEventListener('click', () => {
      for (const node of Object.values(filterInputs)) node.value = '';
      minimum.value = ''; maximum.value = ''; energy.value = '0'; long.checked = false; strict.checked = false; control.edit();
    });
    transfer.addEventListener('click', async () => {
      const captured = control.handoff();
      if (!captured || typeof onedit !== 'function') return;
      transferProblem = ''; control.close();
      const failed = () => {
        const item = controller.selected();
        // The editor normally explains a rejected import itself. Only reopen
        // this snapshot if no editor opened and its original group still fits.
        if (!control.available() || $('direction-dialog')?.open || !item
            || controller.state.selected !== captured.target || C.shape(item.zone) !== captured.signature) return;
        transferProblem = 'The direction editor could not open these settings. Refresh playback or finish its current draft, then try again.';
        control.open();
      };
      try { if (await onedit(captured) === false) failed(); }
      catch (_) { failed(); }
    });
    control.observe();
    return { control, observe: control.observe, suspend: control.suspend, resume: control.resume, dispose: control.dispose };
  }
  return { model, panel, houseTime };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardRehearsal;
