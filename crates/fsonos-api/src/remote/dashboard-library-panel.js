// Cached catalog navigation hands selections to the dashboard's existing review.
'use strict';
const DashboardLibraryPanel = (() => {
  const C = typeof DashboardCore !== 'undefined' ? DashboardCore : require('./dashboard-core.js');
  const L = typeof DashboardLibrary !== 'undefined' ? DashboardLibrary : require('./dashboard-library.js');
  function create({ call, controller, session, doc = document, schedule = setTimeout, cancel = clearTimeout }) {
    const dialog = doc.getElementById('search-dialog');
    if (!dialog) return null;
    const el = (tag, text, cls) => {
      const node = doc.createElement(tag);
      if (text != null) node.textContent = text;
      if (cls) node.className = cls;
      return node;
    };
    const button = (label, action, key = label) => {
      const b = el('button', label, 'outline-button'); b.type = 'button'; b.dataset.catalogKey = key;
      b.addEventListener('click', action); return b;
    };
    const node = el('details', null, 'dashboard-library');
    node.append(el('summary', 'Browse cached library'));
    const body = el('section', null, 'dashboard-library-body');
    body.setAttribute('aria-label', 'Cached music library');
    const scope = el('p', '', 'small-note');
    const kind = el('select'); kind.setAttribute('aria-label', 'Library category');
    for (const [value, label] of [['albums', 'Albums'], ['tracks', 'Tracks'], ['playlists', 'Playlists']]) {
      const option = el('option', label); option.value = value; kind.append(option);
    }
    const filter = el('input'); filter.type = 'search'; filter.maxLength = 1024;
    filter.placeholder = 'Filter this category'; filter.setAttribute('aria-label', 'Filter cached library');
    const refresh = button('Refresh listing', () => { clearTimer(); void control.refresh(); });
    const back = button('Back to albums', () => { clearTimer(); control.edit(''); void control.browse(control.state.playlist ? 'playlists' : 'albums'); });
    const previous = button('Previous page', () => { clearTimer(); void control.previous(); });
    const next = button('Next page', () => { clearTimer(); void control.next(); });
    const status = el('p'); status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite'); status.tabIndex = -1;
    const rows = el('ul', null, 'dashboard-library-items');
    const form = el('form', null, 'dashboard-library-tools');
    const categoryLabel = el('label', 'Category'); categoryLabel.append(kind);
    const filterLabel = el('label', 'Filter'); filterLabel.append(filter);
    form.append(categoryLabel, filterLabel, refresh, back);
    const navigation = el('nav', null, 'dashboard-library-pages'); navigation.setAttribute('aria-label', 'Cached library pages');
    const range = el('span'); navigation.append(previous, range, next);
    let trackDestination = 'group', trackTarget = '', reviewing = false;
    const destinationMode = el('select'); destinationMode.setAttribute('aria-label', 'Selected track destination');
    for (const [value, label] of [['group', 'Current listening group'], ['set', 'Rooms or saved room set']]) {
      const option = el('option', label); option.value = value; destinationMode.append(option);
    }
    const destinationInput = el('input'); destinationInput.type = 'text'; destinationInput.maxLength = 128;
    destinationInput.setAttribute('aria-label', 'Rooms or room set for selected tracks');
    destinationInput.placeholder = 'Saved alias, everywhere, all@S1 or all@S2';
    const destinationLabel = el('label', 'Selected tracks in'); destinationLabel.append(destinationMode);
    const targetLabel = el('label', 'Rooms or room set'); targetLabel.append(destinationInput);
    const destinations = el('div', null, 'dashboard-library-tools'); destinations.append(destinationLabel, targetLabel);
    const selectedCount = el('span');
    const selectedPlay = button('Play selected now', () => { void selectTracks('play'); });
    const selectedNext = button('Queue selected next', () => { void selectTracks('next'); });
    const selectedEnd = button('Add selected to end', () => { void selectTracks('append'); });
    const selectedClear = button('Clear selection', () => control.clearSelection());
    const selectedActions = el('div', null, 'dashboard-library-actions');
    selectedActions.setAttribute('role', 'group'); selectedActions.setAttribute('aria-label', 'Selected track actions');
    selectedActions.append(selectedCount, selectedPlay, selectedNext, selectedEnd, selectedClear);
    const selectionHelp = el('p', `Build a selection of up to ${L.MAX_TRACKS} tracks from different cached albums, playlists and filters. Keep browsing, then move, repeat or remove tracks below before review. Refresh listing, Clear selection or closing this browser clears it.`, 'small-note');
    const selectionNotice = el('p', '', 'small-note'); selectionNotice.setAttribute('role', 'status');
    const basket = el('ol', null, 'dashboard-library-selection');
    basket.setAttribute('aria-label', 'Selected tracks in playback order');
    body.append(el('p', 'Browse the daemon’s saved cache. It may be older or incomplete; browsing does not sync Spotify.', 'small-note'),
      scope, form, status, selectionHelp, selectionNotice, destinations, selectedActions, basket, rows, navigation); node.append(body);
    (doc.getElementById('search-results') || dialog.lastElementChild)?.before(node);
    let disposed = false, timer = null, focusAlbum = false, lastPage = null;
    const visible = () => !disposed && !doc.hidden && dialog.open && node.open && node.isConnected && !node.closest('[hidden], [inert]');
    const control = L.model({ call, changed: render });
    function clearTimer() { if (timer !== null) cancel(timer); timer = null; }
    function close() { clearTimer(); if (reviewing) session.cancel(); node.open = false; control.close(); }
    function select(item, action, destination) {
      if (!visible() || !destination || !controller.canControl() || session.state.pending) return;
      const selected = controller.selected(), choice = control.choose(item);
      if (!choice || !selected || destination.target !== C.key(selected.zone) || destination.shape !== C.shape(selected.zone)) {
        status.textContent = 'The destination or listing changed. Choose the music again.'; return;
      }
      // Close the chooser before opening the existing command-review dialog.
      close(); dialog.close();
      if (!session.reviewLibrary(choice, action, destination)) {
        dialog.showModal(); node.open = true;
        status.textContent = 'The destination changed. Refresh before reviewing music.';
      }
    }
    async function selectTracks(action) {
      if (!visible() || reviewing || !controller.canControl() || session.state.pending) return;
      const selected = controller.selected(), choice = control.chooseTracks();
      if (!selected || !choice) return;
      const destination = Object.freeze(trackDestination === 'set' ? { target: trackTarget, set: true }
        : { target: C.key(selected.zone), shape: C.shape(selected.zone) });
      reviewing = true; render(control.state);
      try {
        const reviewed = await session.reviewLibraryTracks(choice, action, destination);
        if (!visible()) { if (reviewed === session.state.pending) session.cancel(); return; }
        if (reviewed) {
          // Keep the basket through asynchronous review. Only a valid review
          // consumes this chooser; failed reads leave the selection editable.
          reviewing = false; close(); dialog.close();
        } else {
          reviewing = false; render(control.state);
          status.textContent = session.state.error || 'The destination changed. Review the selected tracks again.';
        }
      } finally {
        reviewing = false;
        if (!disposed) {
          render(control.state);
          if (visible() && session.state.error) status.textContent = session.state.error;
        }
      }
    }
    function render(state) {
      if (disposed) return;
      const selected = controller.selected(), destination = selected
        ? Object.freeze({ target: C.key(selected.zone), shape: C.shape(selected.zone) }) : null;
      scope.textContent = destination ? `Actions target ${selected.zone.members.join(' + ')} · ${selected.zone.household}.`
        : 'You can browse without speakers. Choose a listening group before playing or queueing.';
      kind.value = state.kind; if (filter.value !== state.filter) filter.value = state.filter;
      body.setAttribute('aria-busy', String(state.loading));
      refresh.disabled = !visible() || state.loading || reviewing;
      back.hidden = !state.album && !state.playlist;
      back.textContent = state.playlist ? 'Back to playlists' : 'Back to albums';
      kind.disabled = filter.disabled = reviewing;
      const p = state.page, usable = visible() && !state.stale && !state.loading && !state.resetRequired && !reviewing;
      const basketUsable = visible() && !state.loading && !reviewing;
      destinations.hidden = !state.basket.length;
      destinationMode.value = trackDestination; destinationMode.disabled = reviewing || controller.state.busy;
      if (destinationInput.value !== trackTarget) destinationInput.value = trackTarget;
      targetLabel.hidden = trackDestination !== 'set'; destinationInput.disabled = reviewing || controller.state.busy;
      selectionHelp.hidden = selectedActions.hidden = state.kind !== 'tracks' && !state.basket.length;
      selectionNotice.textContent = state.selectionNotice; selectionNotice.hidden = !state.selectionNotice;
      selectedCount.textContent = `${state.basket.length} selected`;
      selectedPlay.disabled = selectedNext.disabled = selectedEnd.disabled = !basketUsable || !state.basket.length || (trackDestination === 'set' ? !trackTarget.trim() : !destination)
        || !controller.canControl() || !!session.state.pending;
      selectedClear.disabled = !visible() || reviewing || !state.basket.length;
      const basketFocus = basket.contains(doc.activeElement) ? doc.activeElement.dataset.catalogKey : null;
      basket.hidden = !state.basket.length;
      basket.replaceChildren();
      for (const [index, selectedRow] of state.basket.entries()) {
        const item = selectedRow.item, name = item.title || 'Untitled track';
        const row = el('li'), info = el('div');
        info.append(el('strong', name));
        const location = item.position ? `Playlist position ${item.position}`
          : selectedRow.page.query.album_uri ? `Disc ${item.disc_number ?? '?'}, track ${item.track_number ?? '?'}`
          : `Listing position ${selectedRow.page.query.offset + selectedRow.index + 1}`;
        info.append(el('p', [item.subtitle, L.selectionOrigin(selectedRow.page), location,
          item.explicit_known === false ? 'Explicit-content metadata unavailable' : item.explicit ? 'Explicit' : ''].filter(Boolean).join(' · '), 'small-note'));
        const actions = el('div', null, 'dashboard-library-actions');
        for (const [label, operation, disabled] of [
          ['Move earlier', () => control.moveTrack(selectedRow.id, -1), index === 0],
          ['Move later', () => control.moveTrack(selectedRow.id, 1), index === state.basket.length - 1],
          ['Repeat track', () => control.repeatTrack(selectedRow.id), state.basket.length >= L.MAX_TRACKS],
          ['Remove selection', () => control.removeTrack(selectedRow.id), false],
        ]) {
          const edit = button(label, () => { if (visible()) operation(); }, `basket:${selectedRow.id}:${label}`);
          edit.setAttribute('aria-label', `${label}: selection ${index + 1}, ${name}`);
          edit.disabled = !basketUsable || disabled; actions.append(edit);
        }
        row.append(info, actions); basket.append(row);
      }
      previous.disabled = !usable || !p || p.query.offset === 0;
      next.disabled = !usable || !p || p.next === null;
      range.textContent = p?.total ? `${p.query.offset + 1}–${p.query.offset + p.items.length} of ${p.total}` : '';
      const parent = state.album || state.playlist;
      const heading = parent ? `Cached tracks in ${parent.title || 'Untitled collection'}` : `Cached ${state.kind}`;
      status.textContent = state.error || (state.loading ? 'Reading cached music…' : state.stale
        ? 'Last observation only. Refresh to choose music.' : `${heading}: ${p?.total || 0} entries.${state.playlist && p ? ` ${L.playlistCoverage(p)}` : state.album && p ? ` ${L.albumCoverage(p)}` : ''}`);
      const focus = rows.contains(doc.activeElement) ? doc.activeElement.dataset.catalogKey : null;
      const scroll = p === lastPage ? rows.scrollTop : 0; lastPage = p;
      rows.replaceChildren();
      for (const [index, item] of (p?.items || []).entries()) {
        const name = item.title || `Untitled ${item.kind}`;
        const row = el('li', null, 'dashboard-library-item'), info = el('div');
        info.append(el('strong', item.position ? `${item.position}. ${name}` : name), el('p', item.subtitle || (state.album ? 'Artist unavailable' : ''), 'small-note'));
        if (item.kind === 'album') info.append(el('p', `${item.cached_tracks} cached track(s); the full album may contain more.`, 'small-note'));
        if (item.album_uri) info.append(el('p', `Disc ${item.disc_number ?? '?'}, track ${item.track_number ?? '?'}`, 'small-note'));
        if (item.explicit_known === false) info.append(el('span', 'Explicit-content metadata unavailable', 'small-note'));
        else if (item.explicit) info.append(el('span', 'Explicit', 'small-note'));
        const actions = el('div', null, 'dashboard-library-actions');
        if (state.kind === 'tracks') {
          const checkbox = el('input'); checkbox.type = 'checkbox'; checkbox.checked = state.selected.includes(index);
          checkbox.dataset.catalogKey = `select:${index}`;
          checkbox.setAttribute('aria-label', `Select ${item.position ? `playlist position ${item.position}, ` : ''}${name}`);
          checkbox.disabled = !usable || (!checkbox.checked && state.basket.length >= L.MAX_TRACKS);
          checkbox.addEventListener('change', () => { if (visible()) control.selectTrack(item, checkbox.checked); });
          const label = el('label', null, 'dashboard-library-select-track'); label.append(checkbox, doc.createTextNode('Select track'));
          info.append(label);
        }
        if (item.kind === 'album' || item.kind === 'playlist') {
          const tracks = button('View cached tracks', () => {
            if (!visible()) return;
            clearTimer(); focusAlbum = true; void control[item.kind](item);
          }, `${item.kind}:${item.source_uri}`);
          tracks.disabled = !usable; actions.append(tracks);
        }
        for (const [action, label] of [['play', 'Play now'], ['append', 'Add to end'], ['next', 'Queue next']]) {
          const b = button(label, () => select(item, action, destination), `${action}:${item.source_uri}:${item.position ?? ''}:${item.disc_number ?? ''}:${item.track_number ?? ''}`);
          b.setAttribute('aria-label', `${label}: ${item.position ? `playlist position ${item.position}, ` : state.album ? `disc ${item.disc_number ?? '?'}, track ${item.track_number ?? '?'}, ` : ''}${name}`);
          b.disabled = !usable || !destination || !controller.canControl() || !!session.state.pending;
          actions.append(b);
        }
        row.append(info, actions); rows.append(row);
      }
      if (p && !p.items.length) {
        const empty = el('li', 'No cached matches. Try another category or refresh your Spotify library.', 'empty');
        const manage = button('Connect or refresh Spotify', () => {}, 'manage-library');
        manage.dataset.openMusic = 'library'; manage.setAttribute('aria-haspopup', 'dialog'); manage.setAttribute('aria-controls', 'music-dialog');
        empty.append(manage); rows.append(empty);
      }
      rows.scrollTop = scroll;
      if (focusAlbum && !state.loading) { focusAlbum = false; if (visible()) status.focus({ preventScroll: true }); }
      else if (focus && visible()) [...rows.querySelectorAll('[data-catalog-key]')].find(b => b.dataset.catalogKey === focus)?.focus({ preventScroll: true });
      else if (basketFocus && visible()) {
        const replacement = [...basket.querySelectorAll('[data-catalog-key]')].find(b => b.dataset.catalogKey === basketFocus);
        if (replacement && !replacement.disabled) replacement.focus({ preventScroll: true });
        else status.focus({ preventScroll: true });
      }
    }
    const toggle = () => {
      clearTimer();
      if (visible()) void control.open(); else control.close();
    };
    node.addEventListener('toggle', toggle); dialog.addEventListener('close', close);
    destinationMode.addEventListener('change', () => {
      if (!visible() || reviewing || controller.state.busy) return;
      trackDestination = destinationMode.value; session.cancel(); render(control.state);
    });
    destinationInput.addEventListener('input', () => {
      if (!visible() || reviewing || controller.state.busy) return;
      trackTarget = destinationInput.value; session.cancel(); render(control.state);
    });
    kind.addEventListener('change', () => { if (visible()) { clearTimer(); void control.browse(kind.value); } });
    filter.addEventListener('input', () => {
      if (!visible()) return;
      clearTimer(); control.edit(filter.value);
      timer = schedule(() => { timer = null; if (visible()) void control.applyFilter(); }, 300);
    });
    form.addEventListener('submit', e => { e.preventDefault(); if (visible()) { clearTimer(); void control.applyFilter(); } });
    render(control.state);
    return { node, model: control, open() {
      if (disposed || doc.hidden || !dialog.open) return;
      clearTimer();
      if (node.open) void control.refresh(); else node.open = true;
      node.querySelector('summary')?.focus();
    }, observe() { render(control.state); },
      suspend() { close(); control.suspend(); }, resume() { control.resume(); },
      dispose() { disposed = true; clearTimer(); control.dispose(); node.removeEventListener('toggle', toggle); dialog.removeEventListener('close', close); node.remove(); },
    };
  }
  return { create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardLibraryPanel;
