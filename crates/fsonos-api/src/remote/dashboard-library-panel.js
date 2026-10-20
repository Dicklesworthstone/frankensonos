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
    const back = button('Back to albums', () => { clearTimer(); control.edit(''); void control.browse('albums'); });
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
    body.append(el('p', 'Browse the daemon’s saved cache. It may be older or incomplete; browsing does not sync Spotify.', 'small-note'),
      scope, form, status, rows, navigation); node.append(body);
    (doc.getElementById('search-results') || dialog.lastElementChild)?.before(node);
    let disposed = false, timer = null, focusAlbum = false, lastPage = null;
    const visible = () => !disposed && !doc.hidden && dialog.open && node.open && node.isConnected && !node.closest('[hidden], [inert]');
    const control = L.model({ call, changed: render });
    function clearTimer() { if (timer !== null) cancel(timer); timer = null; }
    function close() { clearTimer(); node.open = false; control.close(); }
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
    function render(state) {
      if (disposed) return;
      const selected = controller.selected(), destination = selected
        ? Object.freeze({ target: C.key(selected.zone), shape: C.shape(selected.zone) }) : null;
      scope.textContent = destination ? `Actions target ${selected.zone.members.join(' + ')} · ${selected.zone.household}.`
        : 'You can browse without speakers. Choose a listening group before playing or queueing.';
      kind.value = state.kind; if (filter.value !== state.filter) filter.value = state.filter;
      body.setAttribute('aria-busy', String(state.loading));
      refresh.disabled = !visible() || state.loading;
      back.hidden = !state.album;
      const p = state.page, usable = visible() && !state.stale && !state.loading && !state.resetRequired;
      previous.disabled = !usable || !p || p.query.offset === 0;
      next.disabled = !usable || !p || p.next === null;
      range.textContent = p?.total ? `${p.query.offset + 1}–${p.query.offset + p.items.length} of ${p.total}` : '';
      const heading = state.album ? `Cached tracks in ${state.album.title || 'Untitled album'}` : `Cached ${state.kind}`;
      status.textContent = state.error || (state.loading ? 'Reading cached music…' : state.stale
        ? 'Last observation only. Refresh to choose music.' : `${heading}: ${p?.total || 0} entries.`);
      const focus = rows.contains(doc.activeElement) ? doc.activeElement.dataset.catalogKey : null;
      const scroll = p === lastPage ? rows.scrollTop : 0; lastPage = p;
      rows.replaceChildren();
      for (const item of p?.items || []) {
        const name = item.title || `Untitled ${item.kind}`;
        const row = el('li', null, 'dashboard-library-item'), info = el('div');
        info.append(el('strong', name), el('p', item.subtitle, 'small-note'));
        if (item.kind === 'album') info.append(el('p', `${item.cached_tracks} cached track(s); the full album may contain more.`, 'small-note'));
        if (item.album_uri) info.append(el('p', `Disc ${item.disc_number ?? '?'}, track ${item.track_number ?? '?'}`, 'small-note'));
        if (item.explicit) info.append(el('span', 'Explicit', 'small-note'));
        const actions = el('div', null, 'dashboard-library-actions');
        if (item.kind === 'album') {
          const tracks = button('View cached tracks', () => {
            if (!visible()) return;
            clearTimer(); focusAlbum = true; void control.album(item);
          }, `album:${item.source_uri}`);
          tracks.disabled = !usable; actions.append(tracks);
        }
        for (const [action, label] of [['play', 'Play now'], ['append', 'Add to end'], ['next', 'Queue next']]) {
          const b = button(label, () => select(item, action, destination), `${action}:${item.source_uri}`);
          b.setAttribute('aria-label', `${label}: ${name}`);
          b.disabled = !usable || !destination || !controller.canControl() || !!session.state.pending;
          actions.append(b);
        }
        row.append(info, actions); rows.append(row);
      }
      if (p && !p.items.length) rows.append(el('li', 'No cached matches. Try another category or sync the library in the full remote.', 'empty'));
      rows.scrollTop = scroll;
      if (focusAlbum && !state.loading) { focusAlbum = false; if (visible()) status.focus({ preventScroll: true }); }
      else if (focus && visible()) [...rows.querySelectorAll('button')].find(b => b.dataset.catalogKey === focus)?.focus({ preventScroll: true });
    }
    const toggle = () => {
      clearTimer();
      if (visible()) void control.open(); else control.close();
    };
    node.addEventListener('toggle', toggle); dialog.addEventListener('close', close);
    kind.addEventListener('change', () => { if (visible()) { clearTimer(); void control.browse(kind.value); } });
    filter.addEventListener('input', () => {
      if (!visible()) return;
      clearTimer(); control.edit(filter.value);
      timer = schedule(() => { timer = null; if (visible()) void control.refresh(); }, 300);
    });
    form.addEventListener('submit', e => { e.preventDefault(); if (visible()) { clearTimer(); void control.refresh(); } });
    render(control.state);
    return { node, model: control, observe() { render(control.state); },
      suspend() { close(); control.suspend(); }, resume() { control.resume(); },
      dispose() { disposed = true; clearTimer(); control.dispose(); node.removeEventListener('toggle', toggle); dialog.removeEventListener('close', close); node.remove(); },
    };
  }
  return { create };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardLibraryPanel;
