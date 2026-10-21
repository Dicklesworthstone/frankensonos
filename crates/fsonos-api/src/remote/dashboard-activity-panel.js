// Action provenance and checked recovery inside the Listening Room workspace.
'use strict';
const DashboardActivityPanel = (() => {
  function create({ call, controller, session }) {
    const el = (tag, cls, text) => {
      const node = document.createElement(tag);
      if (cls) node.className = cls;
      if (text != null) node.textContent = text;
      return node;
    };
    const button = (text, key, work) => {
      const node = el('button', 'outline-button', text); node.type = 'button';
      node.dataset.activityFocus = key; node.addEventListener('click', work); return node;
    };
    let active = !document.hidden, disposed = false, renderedRows = '', renderedReview = null, renderedResult = null;
    const root = el('details', 'activity-panel recent-section'); root.id = 'dashboard-activity';
    const summary = el('summary', '', 'Activity & undo');
    const body = el('div', 'activity-body');
    const intro = el('p', 'small-note', 'See who changed the house, the policy decision, and the reported result. This is action history, not listening history. Times use this browser’s timezone.');
    const tools = el('div', 'activity-tools');
    const refresh = button('Refresh activity', 'refresh', () => { if (available()) void model.refresh(); });
    const limit = el('select'); limit.setAttribute('aria-label', 'Recent action window');
    for (const n of [20, 100]) { const option = el('option', '', `Newest ${n} actions`); option.value = n; limit.append(option); }
    limit.addEventListener('change', () => { if (available()) model.limit(Number(limit.value)); });
    const searchLabel = el('label', '', 'Find in loaded actions');
    const search = el('input'); search.type = 'search'; search.maxLength = 512;
    search.setAttribute('aria-label', 'Find in loaded actions'); search.placeholder = 'Caller, action, policy or result';
    search.addEventListener('input', () => model.search(search.value)); searchLabel.append(search);
    tools.append(refresh, limit, searchLabel);
    const status = el('p', 'small-note'); status.setAttribute('role', 'status');
    const errors = el('p', 'activity-error'); errors.setAttribute('role', 'alert');
    const list = el('ol', 'activity-list'); list.setAttribute('aria-label', 'Recent house actions');
    const undo = el('section', 'activity-undo'); undo.setAttribute('aria-label', 'Undo the caller’s latest eligible action');
    const candidate = el('div', 'activity-candidate');
    const review = button('Review latest eligible undo', 'review', () => {
      if (!available() || !model.canReview()) return;
      session.cancel(); model.review();
    });
    const reviewBox = el('section', 'activity-review'); reviewBox.setAttribute('aria-label', 'Review checked undo');
    const outcome = el('div', 'activity-outcome'); outcome.setAttribute('role', 'status'); outcome.tabIndex = -1;
    undo.append(el('h3', '', 'Recover an action'), el('p', 'small-note',
      'The daemon chooses the newest eligible action belonging to this caller. It may affect rooms other than the selected group. Other tabs or applications may share this caller identity.'),
      candidate, review, reviewBox, outcome);
    body.append(intro, tools, status, errors, list, undo); root.append(summary, body);
    document.querySelector('.page-footer')?.before(root);
    const model = DashboardActivity.model({ call, controller, changed: render });
    const available = () => active && !disposed && !document.hidden && root.open && !root.inert
      && !root.closest('[hidden], [inert]') && !document.querySelector('dialog[open]');
    function render(s) {
      const busy = s.loading || s.writing || controller.state.busy;
      body.setAttribute('aria-busy', String(busy));
      refresh.disabled = !active || s.writing || controller.state.busy;
      limit.disabled = !active || s.writing; limit.value = String(s.limit);
      if (search.value !== s.query) search.value = s.query;
      errors.textContent = [s.historyError, s.previewError].filter(Boolean).join(' '); errors.hidden = !errors.textContent;
      const rows = model.visible();
      status.textContent = s.loading ? 'Reading action history and undo eligibility…'
        : !s.loaded ? 'No action history has been read.'
          : `${s.stale ? 'Last observation — refresh to check for changes. ' : ''}${rows.length} shown of ${s.rows.length} loaded. This recent window is not the full log.`;
      const rowKey = JSON.stringify([s.rows, s.query, s.loaded]);
      if (rowKey !== renderedRows) {
        renderedRows = rowKey;
        const opened = new Set([...list.querySelectorAll('details[open]')].map(n => n.dataset.actionId));
        const focused = list.contains(document.activeElement) ? document.activeElement?.dataset?.activityEntry : null;
        list.replaceChildren();
        for (const row of rows) {
          const details = el('details'), heading = el('summary', '', `#${row.id} · ${row.intent || '(no intent reported)'}`);
          details.dataset.actionId = String(row.id); details.open = opened.has(String(row.id)); heading.dataset.activityEntry = String(row.id);
          const stamp = el('time', '', new Date(row.at * 1000).toLocaleString()); stamp.dateTime = new Date(row.at * 1000).toISOString();
          const facts = el('dl');
          for (const [label, value] of [['Caller / surface', `${row.client} / ${row.surface}`], ['Policy', row.decision], ['Result', row.result]]) {
            facts.append(el('dt', '', label), el('dd', '', value));
          }
          details.append(heading, stamp, facts, el('p', 'small-note', row.undo_of !== null
            ? `Undo attempt for #${row.undo_of}. Its result may describe incomplete restoration.`
            : row.undoable ? 'A before-state was recorded. This is not proof of current undo eligibility.'
              : 'No undoable before-state was recorded. Queue removal and reordering have no redo/undo here.'));
          const li = el('li'); li.append(details); list.append(li);
        }
        if (s.loaded && !rows.length) list.append(el('li', 'empty', s.rows.length ? 'No loaded actions match.' : 'No actions were returned.'));
        if (focused) [...list.querySelectorAll('summary')].find(n => n.dataset.activityEntry === focused)?.focus({ preventScroll: true });
      }
      candidate.replaceChildren();
      if (s.previewLoaded && s.candidate) {
        const c = s.candidate;
        candidate.append(el('strong', '', `#${c.id} · ${c.intent}`), el('p', '', `Caller: ${c.client} · ${c.surface}`), el('p', '', `Original result: ${c.result}`));
      } else candidate.append(el('p', '', s.previewLoaded ? 'No eligible action for this caller.' : 'Undo eligibility is not current. Refresh activity to inspect it.'));
      review.disabled = !available() || busy || !!s.pending || !model.canReview() || !!session.state.pending;
      if (controller.state.uncertain) candidate.append(el('p', 'activity-error', 'A previous command is unconfirmed. Use the dashboard’s main Refresh before another command; refreshing this log does not resolve it.'));
      else if (s.candidate && !controller.canControl()) candidate.append(el('p', 'small-note', 'Choose a group with fresh playback state before confirming recovery. The action can affect other groups.'));
      reviewBox.hidden = !s.pending;
      const reviewHadFocus = reviewBox.contains(document.activeElement);
      if (renderedReview !== s.pending) {
        renderedReview = s.pending; reviewBox.replaceChildren();
        if (s.pending) {
          const pending = s.pending, c = pending.action;
          const cancel = button('Keep current state', 'cancel', () => model.cancel());
          const confirm = button(`Confirm undo #${c.id}`, 'confirm', () => {
            if (available() && !session.state.pending) void model.confirm(pending);
          });
          reviewBox.append(el('h3', '', `Undo #${c.id}: ${c.intent}`), el('p', '', `Caller: ${c.client}. Original result: ${c.result}`),
            el('p', '', 'Restore the state saved before this action. This is not limited to the selected room. Current policy, offline speakers, or intervening changes may prevent parts of the restore. There is no redo.'),
            el('p', '', 'The daemon checks this exact action again. A different eligible action is refused, never substituted.'), cancel, confirm);
          cancel.focus({ preventScroll: true });
        } else if (reviewHadFocus) (s.writing ? outcome : review).focus({ preventScroll: true });
      }
      if (renderedResult !== s.result || outcome.dataset.message !== s.message || outcome.dataset.writing !== String(s.writing)) {
        renderedResult = s.result; outcome.dataset.message = s.message; outcome.dataset.writing = String(s.writing); outcome.replaceChildren();
        if (s.writing) outcome.append(el('p', '', 'Checking and submitting the reviewed undo…'));
        else if (s.message) outcome.append(el('p', '', s.message));
        if (s.result?.failures.length) {
          const failures = el('ul'); failures.setAttribute('aria-label', 'Restoration failures');
          for (const reason of s.result.failures) failures.append(el('li', '', reason));
          outcome.append(failures);
        }
      }
    }
    root.addEventListener('toggle', e => {
      if (e.target !== root) return;
      if (root.open && active) void model.refresh(); else model.close();
    });
    if (!active) model.suspend();
    render(model.state);
    return { node: root, model,
      observe() { if (session.state.pending && model.state.pending) model.cancel(); model.observe(); },
      invalidate: model.invalidate,
      suspend() { active = false; model.suspend(); },
      resume() { if (!disposed) { active = true; model.resume(); } },
      dispose() { disposed = true; model.dispose(); },
    };
  }
  return { create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardActivityPanel;
