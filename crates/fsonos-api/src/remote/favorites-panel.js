// One favorites editor on the phone and dashboard. UI changes never submit a
// catalog mutation; only a captured review's confirmation can do that.
'use strict';
const FavoritesPanel = (() => {
  const F = typeof FavoritesRemote !== 'undefined' ? FavoritesRemote : require('./favorites.js');
  const P = typeof SourcePlayerRemote !== 'undefined' ? SourcePlayerRemote : require('./source-player.js');

  function panel({ call, document: doc = document, showTargets = true, changed = () => {}, ...options }) {
    let control, disposed = false, opener = null, shownReview = null, shownResult = null, renderedCatalog = null;
    const listeners = [], rowButtons = [];
    const on = (node, name, fn) => { node.addEventListener(name, fn); listeners.push([node, name, fn]); };
    const el = (tag, content, className) => {
      const node = doc.createElement(tag);
      if (content != null) node.textContent = content;
      if (className) node.className = className;
      return node;
    };
    const button = (id, label, run) => {
      const node = el('button', label, 'outline-button'); node.id = id; node.type = 'button';
      if (run) on(node, 'click', run); return node;
    };
    const trigger = button('open-favorites', 'Sonos favorites', () => { void open(); });
    trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-controls', 'favorites-dialog');
    const dialog = el('dialog', null, 'favorites-dialog'); dialog.id = 'favorites-dialog';
    dialog.setAttribute('aria-labelledby', 'favorites-title');
    const heading = el('h2', 'Keep your favorite music.'); heading.id = 'favorites-title';
    const close = button('favorites-close', 'Close', () => control.close());
    const header = el('header'); header.append(heading, close);
    const intro = el('p', 'Save the actual selected track or radio station as a Sonos favorite, or rename and remove an exact saved entry. Favorites belong to one household.', 'small-note');
    const demoNote = el('p', 'This preview cannot read or change your real favorites. Open the live daemon to manage them.', 'favorites-notice'); demoNote.hidden = !options.demo;
    const targetLabel = el('label', 'Listening group'); targetLabel.htmlFor = 'favorites-target';
    const target = el('select'); target.id = 'favorites-target'; targetLabel.append(target); targetLabel.hidden = !showTargets;
    const scope = el('p', '', 'favorites-scope'); scope.id = 'favorites-scope';
    const read = button('favorites-refresh', 'Read favorites', () => { void control.refresh(); });
    const save = button('favorites-current', 'Save current music…', () => { void control.readCurrent(); });
    const actions = el('div', null, 'favorites-actions'); actions.append(read, save);
    const message = el('p', '', 'favorites-message'); message.id = 'favorites-message';
    message.setAttribute('role', 'status'); message.setAttribute('aria-live', 'polite');
    const editor = el('form', null, 'favorites-editor'); editor.id = 'favorites-form';
    const source = el('p', '', 'favorites-source'); source.id = 'favorites-source';
    const nameLabel = el('label', 'Favorite name'); nameLabel.htmlFor = 'favorites-name';
    const name = el('input'); name.id = 'favorites-name'; name.type = 'text'; name.maxLength = 8192; name.autocomplete = 'off';
    nameLabel.append(name);
    const effect = el('p', '', 'small-note'); effect.id = 'favorites-effect';
    const review = button('favorites-review', 'Review favorite', null); review.type = 'submit';
    editor.append(source, nameLabel, effect, review);
    const preview = el('section', null, 'favorites-review'); preview.id = 'favorites-preview'; preview.tabIndex = -1;
    preview.setAttribute('aria-label', 'Review favorite change');
    const previewText = el('p'), previewSource = el('p'), previewScope = el('p'), previewEffect = el('p', '', 'small-note');
    previewText.id = 'favorites-preview-title'; previewSource.id = 'favorites-preview-source'; previewScope.id = 'favorites-preview-group';
    const back = button('favorites-back', 'Go back', () => control.cancel());
    const confirm = button('favorites-confirm', 'Confirm favorite', () => { void control.confirm(shownReview); });
    const reviewActions = el('div', null, 'favorites-actions'); reviewActions.append(back, confirm);
    preview.append(previewText, previewSource, previewScope, previewEffect, reviewActions);
    const result = el('section', null, 'favorites-result'); result.id = 'favorites-result'; result.tabIndex = -1;
    result.setAttribute('role', 'status'); result.setAttribute('aria-live', 'polite');
    const resultTitle = el('h3'), resultScope = el('p'), resultDetail = el('p'), resultId = el('code');
    resultTitle.id = 'favorites-result-title'; resultScope.id = 'favorites-result-group'; resultDetail.id = 'favorites-result-detail'; resultId.id = 'favorites-result-id';
    const recover = button('favorites-recover', 'Read original household favorites', () => { void control.recover(); });
    const recoveryCatalog = el('div', null, 'favorites-recovery-catalog'); recoveryCatalog.id = 'favorites-recovery-catalog';
    result.append(resultTitle, resultScope, resultId, resultDetail, recover, recoveryCatalog);
    const catalogHeading = el('h3', 'Saved favorites');
    const filterLabel = el('label', 'Find a saved favorite'); filterLabel.htmlFor = 'favorites-filter';
    const filter = el('input'); filter.id = 'favorites-filter'; filter.type = 'search'; filter.maxLength = 512; filter.autocomplete = 'off'; filterLabel.append(filter);
    const summary = el('p', '', 'small-note'); summary.id = 'favorites-catalog-summary';
    const list = el('ul', null, 'favorites-list'); list.id = 'favorites-list'; list.setAttribute('aria-label', 'Exact household favorites');
    dialog.append(header, intro, demoNote, targetLabel, scope, actions, message, editor, preview, result, catalogHeading, filterLabel, summary, list);
    doc.body.append(dialog);
    control = F.model({ call, ...options, changed: state => { render(state); changed(state); } });

    function renderCatalog(state) {
      const key = [state.catalog, state.catalogFresh, filter.value];
      if (renderedCatalog && key.every((value, i) => value === renderedCatalog[i])) return;
      renderedCatalog = key; rowButtons.length = 0; list.replaceChildren();
      const words = filter.value.toLocaleLowerCase().trim().split(/\s+/).filter(Boolean);
      const matches = state.catalog.filter(item => words.every(word => `${item.title} ${item.description || ''} ${item.id}`.toLocaleLowerCase().includes(word)));
      summary.textContent = !state.catalogFresh ? 'Read the current household favorites before making a change.'
        : !state.catalog.length ? 'This household has no saved favorites. Save a selected track or station above.'
          : `${matches.length} matching favorite(s) in ${state.group?.target || 'this household'}.${matches.length > 100 ? ' Showing the first 100; narrow the filter to find another entry.' : ''}`;
      for (const item of matches.slice(0, 100)) {
        const row = el('li'), details = el('div'), title = el('strong', item.title);
        details.append(title, el('p', `${item.kind} · ${item.id}`, 'small-note'));
        if (item.description) details.append(el('p', item.description, 'small-note'));
        const rename = el('button', 'Rename', 'text-button'), remove = el('button', 'Remove', 'text-button');
        rename.type = remove.type = 'button';
        rename.setAttribute('aria-label', `Rename ${item.title} (${item.id})`);
        remove.setAttribute('aria-label', `Remove ${item.title} (${item.id})`);
        rename.addEventListener('click', () => { if (control.select(item)) name.focus(); });
        remove.addEventListener('click', () => { if (control.select(item)) void control.review('remove'); });
        rowButtons.push(rename, remove); const buttons = el('div', null, 'favorites-actions'); buttons.append(rename, remove);
        row.append(details, buttons); list.append(row);
      }
    }
    function render(state) {
      if (disposed) return;
      trigger.textContent = state.recovery ? 'Check favorite change' : 'Sonos favorites';
      trigger.setAttribute('aria-expanded', String(state.open));
      if (state.open && !dialog.open) { opener = doc.activeElement; dialog.showModal(); }
      if (!state.open && dialog.open) { dialog.close(); if (opener?.isConnected) opener.focus({ preventScroll: true }); }
      const targets = JSON.stringify(state.groups.map(group => [group.target, group.signature]));
      if (target.dataset.groups !== targets) {
        target.dataset.groups = targets; target.replaceChildren();
        const blank = el('option', 'Choose a group…'); blank.value = ''; target.append(blank);
        for (const group of state.groups) { const option = el('option', group.label); option.value = group.target; target.append(option); }
      }
      target.value = state.group?.target || ''; target.disabled = control.busy() || !state.active;
      scope.textContent = state.group ? `Household group: ${state.group.label}` : 'Choose an available listening group.';
      message.textContent = state.message;
      read.disabled = !control.canRead(); save.disabled = !control.editable() || !state.group;
      editor.hidden = !state.current && !state.item;
      if (name.value !== state.title) name.value = state.title;
      name.disabled = !control.editable(); review.disabled = !control.canReview();
      const item = state.current?.favorite || state.item;
      source.textContent = item ? `${state.current ? 'Observed current source' : 'Exact saved favorite'}: ${item.title} · ${item.kind}${item.id ? ' · ' + item.id : ''}${item.description ? ' · ' + item.description : ''}` : '';
      effect.textContent = state.current ? 'Creates a new favorite for this selected track or station. The complete queue is not saved. A matching name does not replace an existing favorite.'
        : 'Renames this exact household entry. Its saved music and current playback stay unchanged.';
      review.textContent = state.current ? 'Review new favorite' : 'Review rename';
      preview.hidden = !state.pending;
      if (shownReview !== state.pending) {
        shownReview = state.pending;
        if (shownReview) {
          const p = shownReview;
          previewText.textContent = p.operation === 'save' ? `Create “${p.title}”?` : p.operation === 'rename' ? `Rename “${p.item.title}” to “${p.title}”?` : `Remove “${p.item.title}”?`;
          previewSource.textContent = `${p.item.kind}: ${p.item.title}${p.item.id ? ' (' + p.item.id + ')' : ''}`;
          previewScope.textContent = `Original group: ${p.group.label}`;
          previewEffect.textContent = p.operation === 'remove'
            ? 'Remove this exact favorite from its household. Current music and queues keep playing. Saved scenes or routines referring to it may need updating. This catalog deletion cannot be undone here.'
            : p.operation === 'save' ? 'Save the observed track or station, leaving playback and the queue untouched. The exact source is checked again before the one save request.'
              : 'Change only this favorite’s name. Saved scenes or routines that match its old title may need updating. Playback and queues stay unchanged.';
          confirm.textContent = p.operation === 'save' ? 'Create Sonos favorite' : p.operation === 'rename' ? 'Rename favorite' : 'Remove favorite';
          if (state.open) preview.focus({ preventScroll: true });
        }
      }
      confirm.disabled = control.busy() || !state.pending; back.disabled = state.writing;
      result.hidden = !state.result;
      if (state.result) {
        const r = state.result;
        resultTitle.textContent = r.status === 'accepted' ? ({ save: 'Favorite saved', rename: 'Favorite renamed', remove: 'Favorite removed' }[r.operation])
          : r.status === 'uncertain' ? 'Favorite change needs inspection' : r.status === 'observed' ? 'Original household inspected' : 'Favorite change was not sent';
        resultScope.textContent = r.group.target; resultDetail.textContent = r.detail; resultId.textContent = r.favorite?.id || r.item.id;
        if (shownResult !== r) { shownResult = r; if (state.open) result.focus({ preventScroll: true }); }
      }
      recover.hidden = !state.recovery; recover.disabled = !control.canRead();
      recoveryCatalog.replaceChildren(); recoveryCatalog.hidden = !state.recoveryCatalog;
      if (state.recoveryCatalog) {
        const recovered = state.recoveryCatalog;
        recoveryCatalog.append(el('h4', 'Observed favorites in ' + recovered.group.target));
        if (!recovered.items.length) recoveryCatalog.append(el('p', 'No favorites were returned for this household.'));
        for (const entry of recovered.items.slice(0, 100)) recoveryCatalog.append(el('p', `${entry.title} · ${entry.kind} · ${entry.id}`));
        if (recovered.items.length > 100) recoveryCatalog.append(el('p', `${recovered.items.length} favorites were observed. Open this household’s catalog and filter it to inspect later entries.`));
      }
      renderCatalog(state);
      for (const button of rowButtons) button.disabled = !control.editable() || !state.catalogFresh;
      filter.disabled = state.writing;
    }
    async function open(target) {
      if (disposed) return false;
      const other = doc.querySelector?.('dialog[open]');
      if (other && other !== dialog) return false;
      return control.open(target);
    }
    on(target, 'change', () => { if (target.value) void control.refresh(target.value); });
    on(name, 'input', () => control.editTitle(name.value));
    on(filter, 'input', () => render(control.state));
    on(editor, 'submit', event => { event.preventDefault(); void control.review(control.state.current ? 'save' : 'rename'); });
    on(dialog, 'cancel', event => { event.preventDefault(); control.close(); });
    on(dialog, 'close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return { trigger, dialog, control, open, observe: () => control.observe(),
      catalogChanged: change => control.catalogChanged(change), suspend: () => control.suspend(), resume: () => control.resume(),
      dispose() { if (disposed) return; control.close(); control.dispose(); disposed = true; for (const [node, event, fn] of listeners) node.removeEventListener(event, fn); } };
  }

  function dashboard({ call, controller, demo = false, completed = () => {}, document: doc = document }) {
    const view = panel({ call, demo, document: doc, showTargets: false,
      selection() { try { return controller.selected() ? P.groups([controller.selected().zone])[0] : null; } catch (_) { return null; } },
      available: () => controller.state.topologyFresh && controller.canControl(),
      readable: () => controller.state.active && !controller.state.busy,
      async commit(reviewed, work) {
        const ok = await controller.mutate(work, { stored: 'favorites', storeTarget: reviewed.group.target });
        if (!ok) throw new Error(controller.state.message || 'The favorite command was not submitted.');
      },
      readCatalog: (storeTarget, decode) => controller.refreshStored('favorites', decode, { explicit: true, storeTarget }), completed,
    });
    doc.getElementById('open-source')?.parentElement?.append(view.trigger);
    return view;
  }

  function phone({ call, cards, current, busy, begin, end, register, catalogChanged = () => {}, document: doc = document }) {
    const request = F.phoneClient(call), listeners = [];
    const on = (node, event, fn) => { if (node) { node.addEventListener(event, fn); listeners.push([node, event, fn]); } };
    let view, held = null, disposed = false, studio = null;
    const partsFor = group => [...cards()].find(parts => current(parts)
      && P.groups([parts.zone])[0].signature === group?.signature);
    view = panel({ call: request, document: doc, demo: doc.body.dataset.preview === 'true',
      available() { const parts = partsFor(view?.control.state.group); return !!parts && !parts.writing && !busy(parts); },
      async commit(reviewed, work) {
        const parts = partsFor(reviewed.group);
        if (!parts || !begin(parts)) throw new Error('The original group changed or another command is using it. Read and review again.');
        held = parts;
        try {
          return await work({ check() {
            if (!current(parts) || partsFor(reviewed.group) !== parts) throw new Error('The original group changed before the favorite command.');
          }, send: request });
        } finally { held = null; end(parts); }
      },
      completed(result) {
        const change = { household: result.group.household, favorites: true, playlists: false };
        // The existing per-card picker has its own coalesced catalog cache.
        // Refresh it immediately after a confirmed local edit, even when the
        // speaker's catalog event is delayed or unavailable.
        try { catalogChanged(change); } catch (_) { /* Keep the confirmed receipt. */ }
        const Event = doc.defaultView?.CustomEvent || globalThis.CustomEvent;
        doc.dispatchEvent(new Event('fsonos:catalog-changed', { detail: change }));
      },
    });
    const section = doc.createElement('section'); section.id = 'favorites-management'; section.className = 'scenes favorites-entry';
    section.setAttribute('aria-label', 'Sonos favorites');
    const title = doc.createElement('h2'); title.textContent = 'Sonos favorites';
    const description = doc.createElement('p'); description.textContent = 'Keep the selected track or station, rename an entry, or remove a saved favorite from its household.';
    section.append(title, description, view.trigger); doc.getElementById('households')?.before(section);
    register?.(parts => {
      const button = doc.createElement('button'); button.type = 'button'; button.textContent = 'Manage favorites';
      button.setAttribute('aria-haspopup', 'dialog'); button.setAttribute('aria-controls', 'favorites-dialog');
      const open = () => { if (!button.disabled) void view.open(P.groups([parts.zone])[0].target); };
      button.addEventListener('click', open);
      const observe = () => {
        button.disabled = !current(parts) || parts.writing || busy(parts) || !view.control.state.active;
        if (held !== parts) view.observe();
      };
      observe();
      return { node: button, observe, block: observe,
        stale() { if (view.control.state.group?.signature === P.groups([parts.zone])[0].signature) view.control.invalidate('Read the group again before changing favorites.'); },
        dispose() { button.removeEventListener('click', open); if (view.control.state.group?.signature === P.groups([parts.zone])[0].signature) view.control.invalidate('The original group changed. Read its favorites again.'); },
      };
    });
    on(doc, 'fsonos:catalog-changed', event => view.catalogChanged(event.detail));
    on(doc, 'fsonos:studio-selection', event => {
      const next = JSON.stringify([event.detail?.key, event.detail?.workspace]);
      if (studio !== null && studio !== next) view.control.close();
      studio = next;
    });
    on(doc, 'visibilitychange', () => { if (doc.hidden) view.suspend(); else view.resume(); });
    on(doc.defaultView, 'pagehide', () => view.suspend());
    on(doc.defaultView, 'pageshow', () => { if (!doc.hidden) view.resume(); });
    if (doc.hidden) view.suspend();
    return { ...view, node: section, dispose() {
      if (disposed) return; disposed = true; view.dispose(); for (const [node, event, fn] of listeners) node.removeEventListener(event, fn);
    } };
  }
  return { panel, dashboard, phone };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = FavoritesPanel;
else if (typeof registerCardPanel === 'function') {
  Object.assign(ROUTES, FavoritesRemote.routes);
  FavoritesPanel.phone({ call, cards: () => house.cards.values(), register: registerCardPanel,
    current: ownsCard, busy: transportBusy, begin: beginAction, end: endAction,
    catalogChanged: change => catalogs.invalidate(change) });
}
