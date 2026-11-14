// Save the selected group's retained queue through the native reviewed model.
'use strict';
const DashboardPlaylistSavePanel = (() => {
  const S = typeof DashboardPlaylistSave !== 'undefined' ? DashboardPlaylistSave : require('./dashboard-playlist-save.js');
  function panel({ call, controller, saved, browse = () => false, changed = () => {},
    prepareRead = null, canPrepareRead = () => false, demo = false, document: doc = document, now }) {
    const dialog = doc.getElementById('playlist-save-dialog'), trigger = doc.getElementById('open-playlist-save');
    if (!dialog || !trigger) throw new Error('The save-queue dialog or trigger is missing.');
    let control, disposed = false, opener = null, shownReview = null, shownResult = null, shownCatalog = null, reviewFocus = null;
    let viewMessage = '', browsing = false, preparing = false, preparation = 0;
    const listeners = [];
    const on = (node, event, fn) => { node.addEventListener(event, fn); listeners.push([node, event, fn]); };
    const el = (tag, text, className) => {
      const node = doc.createElement(tag);
      if (text != null) node.textContent = text;
      if (className) node.className = className;
      return node;
    };
    const button = (id, text, run, className = 'outline-button') => {
      const node = el('button', text, className); node.id = id; node.type = 'button';
      if (run) on(node, 'click', run); return node;
    };
    const heading = el('h2', 'Save this queue for later.'); heading.id = 'playlist-save-title';
    const header = el('header');
    const closeButton = button('close-playlist-save', 'Close', () => control.close(), 'text-button');
    closeButton.setAttribute('aria-label', 'Close save queue');
    header.append(el('span', 'A GOOD SET, READY AGAIN', 'eyebrow'), closeButton);
    const description = el('p', 'Create a new Sonos playlist from this group’s retained queue. It stays saved on this household’s speakers.', 'small-note');
    description.id = 'playlist-save-description';
    const demoNote = el('p', 'The design preview cannot read a real queue or save a playlist. Open your live daemon to save one.', 'playlist-save-notice');
    demoNote.hidden = !demo;
    const context = el('section', null, 'playlist-save-context');
    const destination = el('strong'); destination.id = 'playlist-save-target';
    const observed = el('p', '', 'small-note'); observed.id = 'playlist-save-observation';
    context.append(destination, observed);
    const read = button('playlist-save-read', 'Read current queue', () => { viewMessage = ''; void readQueue(); });
    const retained = el('p', 'You can save a retained queue while a stream or radio plays. Saving does not start, stop or rearrange music, or change the DJ.', 'small-note');
    const form = el('form', null, 'playlist-save-form'); form.id = 'playlist-save-form';
    const label = el('label', 'Playlist name'); label.htmlFor = 'playlist-save-name';
    const name = el('input'); name.id = 'playlist-save-name'; name.type = 'text'; name.autocomplete = 'off'; name.maxLength = 8192;
    name.placeholder = 'Sunday evening'; name.setAttribute('aria-describedby', 'playlist-save-name-help');
    const nameHelp = el('p', '', 'small-note'); nameHelp.id = 'playlist-save-name-help';
    const createOnly = el('p', 'This always creates a new playlist. A matching name does not replace an existing playlist.', 'small-note');
    const reviewButton = button('playlist-save-review', 'Review new playlist', null, 'primary-button'); reviewButton.type = 'submit';
    const actions = el('div', null, 'playlist-save-actions'); actions.append(read, reviewButton);
    form.append(label, name, nameHelp, createOnly, actions);
    const message = el('p', '', 'playlist-save-message'); message.id = 'playlist-save-message';
    message.setAttribute('role', 'status'); message.setAttribute('aria-live', 'polite');
    const preview = el('section', null, 'playlist-save-review'); preview.id = 'playlist-save-preview'; preview.tabIndex = -1;
    preview.setAttribute('aria-labelledby', 'playlist-save-preview-title');
    const previewTitle = el('h3', 'Create this playlist?'); previewTitle.id = 'playlist-save-preview-title';
    const spec = el('dl');
    const reviewedName = el('dd'), reviewedTarget = el('dd'), reviewedQueue = el('dd');
    reviewedName.id = 'playlist-save-preview-name'; reviewedTarget.id = 'playlist-save-preview-target'; reviewedQueue.id = 'playlist-save-preview-queue';
    spec.append(el('dt', 'Name'), reviewedName, el('dt', 'Listening group'), reviewedTarget, el('dt', 'Observed queue'), reviewedQueue);
    const back = button('playlist-save-back', 'Go back', () => control.cancelReview(), 'text-button');
    const confirm = button('playlist-save-confirm', 'Create Sonos playlist', () => { viewMessage = ''; void control.apply(); }, 'primary-button');
    const reviewActions = el('footer', null, 'playlist-save-actions'); reviewActions.append(back, confirm);
    preview.append(previewTitle, spec,
      el('p', 'A new playlist is created even if this name already exists. Playback and the retained queue keep their current state.', 'small-note'),
      el('p', 'The group and queue are checked again before saving. Another controller can still change the queue; Sonos copies its contents when the command runs.', 'small-note'), reviewActions);
    const result = el('section', null, 'playlist-save-result'); result.id = 'playlist-save-result'; result.tabIndex = -1;
    result.setAttribute('aria-labelledby', 'playlist-save-result-title');
    const resultTitle = el('h3'); resultTitle.id = 'playlist-save-result-title';
    const resultName = el('strong'); resultName.id = 'playlist-save-result-name';
    const identity = el('code'); identity.id = 'playlist-save-result-id';
    const resultTarget = el('p', '', 'small-note'); resultTarget.id = 'playlist-save-result-target';
    const resultDetail = el('p', '', 'small-note'); resultDetail.id = 'playlist-save-result-detail';
    const logWarning = el('p', 'The playlist is saved, but its action-log entry could not be written. Do not repeat the save to repair the log.', 'playlist-save-notice');
    logWarning.id = 'playlist-save-log-warning';
    const recovery = el('p', '', 'playlist-save-notice'); recovery.id = 'playlist-save-recovery';
    const inspect = button('playlist-save-check-catalog', 'Check saved playlists', () => { viewMessage = ''; void control.checkCatalog(); });
    const browseButton = button('playlist-save-browse', 'Browse saved playlists', () => { void browseSaved(); });
    const browseHelp = el('p', '', 'small-note'); browseHelp.id = 'playlist-save-browse-help';
    const resultActions = el('div', null, 'playlist-save-actions'); resultActions.append(inspect, browseButton);
    result.append(resultTitle, resultName, identity, resultTarget, resultDetail, logWarning, recovery, resultActions, browseHelp);
    const catalog = el('section', null, 'playlist-save-catalog'); catalog.id = 'playlist-save-catalog';
    catalog.setAttribute('aria-labelledby', 'playlist-save-catalog-title');
    const catalogTitle = el('h3', 'Saved playlists in the original household'); catalogTitle.id = 'playlist-save-catalog-title';
    const catalogScope = el('p', '', 'small-note'); catalogScope.id = 'playlist-save-catalog-scope';
    const catalogSummary = el('p', '', 'small-note'); catalogSummary.id = 'playlist-save-catalog-summary';
    const catalogItems = el('ul'); catalogItems.id = 'playlist-save-catalog-items';
    catalog.append(catalogTitle, catalogScope, catalogSummary, catalogItems);
    dialog.replaceChildren(header, heading, description, demoNote, context, retained, form, message, preview, result, catalog);
    dialog.setAttribute('aria-describedby', description.id);

    function titleHelp(value) {
      if (!value) return 'Use a memorable title. Names can use up to ' + S.MAX_TITLE_BYTES + ' UTF-8 bytes.';
      try {
        const title = S.titleOf(value), bytes = new TextEncoder().encode(title).length;
        return bytes + ' of ' + S.MAX_TITLE_BYTES + ' UTF-8 bytes.' + (title !== value ? ' Spaces at the beginning and end will be removed.' : '');
      } catch (error) { return error.message; }
    }
    const canReadQueue = () => !preparing && (control.canRead()
      || (typeof prepareRead === 'function' && control.state.open && !control.state.loading
        && !control.state.writing && control.state.catalogStatus !== 'loading' && canPrepareRead()));
    async function readQueue() {
      if (!canReadQueue()) return false;
      if (control.canRead()) return control.read();
      const token = ++preparation, target = controller.state.selected, scope = control.state.scope;
      preparing = true; control.invalidate(); render(control.state);
      try {
        if (await prepareRead() !== true || disposed || token !== preparation || !control.state.open
            || target !== controller.state.selected || scope !== control.state.scope) return false;
        return await control.read();
      } catch (error) {
        if (!disposed && token === preparation && control.state.open) viewMessage = error.message || 'The listening group could not be refreshed.';
        return false;
      } finally { preparing = false; render(control.state); }
    }
    function render(s) {
      if (disposed || !control) return;
      if (!s.open) ++preparation;
      trigger.disabled = !controller.state.active || (controller.state.busy && !s.writing)
        || (!controller.selected() && !s.result);
      trigger.setAttribute('aria-expanded', String(s.open));
      if (s.open && !dialog.open) dialog.showModal();
      else if (!s.open && dialog.open) {
        dialog.close(); if (opener?.isConnected && !opener.disabled) opener.focus();
      }
      dialog.setAttribute('aria-busy', String(preparing || s.loading || s.writing || s.catalogStatus === 'loading'));
      destination.textContent = s.group?.label || 'Select a listening group to save its queue.';
      observed.textContent = s.loading ? 'Reading this group and its retained queue…' : s.queue
        ? (s.loaded ? 'Observed queue: ' : 'Last queue observation: ') + s.queue.total.toLocaleString() + ' ' + (s.queue.total === 1 ? 'entry.' : 'entries.')
          + (!s.queue.total ? ' Add music to this queue before saving.' : !s.loaded ? ' Read it again before a new save.' : '')
        : 'Read the current queue to see what you are saving.';
      name.disabled = preparing || !control.editable();
      if (name.value !== s.title) name.value = s.title;
      nameHelp.textContent = titleHelp(s.title);
      read.disabled = !canReadQueue(); reviewButton.disabled = preparing || !control.canReview();
      message.textContent = viewMessage || s.message;
      preview.hidden = !s.pending;
      if (s.pending) {
        reviewedName.textContent = s.pending.title;
        reviewedTarget.textContent = s.pending.group.label;
        reviewedQueue.textContent = s.pending.queue.total.toLocaleString() + ' ' + (s.pending.queue.total === 1 ? 'entry' : 'entries');
      }
      confirm.disabled = preparing || !control.canApply(); back.disabled = preparing || s.writing;
      const receipt = s.result, uncertain = receipt?.status === 'uncertain', accepted = receipt?.status === 'accepted';
      result.hidden = !receipt;
      result.dataset.status = receipt?.status || '';
      resultTitle.textContent = accepted ? 'Playlist saved' : uncertain ? 'Check the save outcome' : 'No save was sent';
      resultName.textContent = receipt?.playlist?.title || receipt?.title || '';
      identity.hidden = !receipt?.playlist; identity.textContent = receipt?.playlist?.id || '';
      resultTarget.textContent = receipt ? (receipt.group?.label || receipt.label || receipt.target) + ' · ' + receipt.target : '';
      resultDetail.textContent = receipt?.detail || '';
      logWarning.hidden = !accepted || receipt.logged !== false;
      recovery.hidden = !uncertain;
      const checked = uncertain && s.catalogStatus === 'ready' && s.catalogTarget === receipt.target;
      recovery.textContent = checked
        ? 'The original household’s catalog has been read. A matching name cannot confirm which request created it. Nothing was retried; read the queue and review any new save explicitly.'
        : 'This save may have completed. Check the saved catalog for the original group before another save. The request will not be repeated automatically.';
      inspect.hidden = !uncertain; inspect.disabled = !control.canCheckCatalog();
      inspect.textContent = s.catalogStatus === 'loading' ? 'Checking saved playlists…' : checked ? 'Check catalog again' : 'Check saved playlists';
      browseButton.hidden = !accepted;
      const sameTarget = receipt && controller.state.selected === receipt.target;
      browseButton.disabled = !accepted || !sameTarget || !controller.canControl() || s.writing || browsing;
      browseHelp.hidden = !accepted;
      browseHelp.textContent = sameTarget
        ? 'Browse the household’s native catalog, then review playback separately.'
        : 'Select ' + (receipt?.target || 'the saved group') + ' to browse its playlists. The saved receipt stays with its original household.';
      catalog.hidden = s.catalogStatus !== 'ready';
      if (s.catalogStatus === 'ready' && shownCatalog !== s.catalog) {
        shownCatalog = s.catalog; catalogItems.replaceChildren();
        catalogScope.textContent = 'Read for ' + s.catalogTarget + '. This is a catalog observation, not a save receipt.';
        const matches = s.catalog.filter(item => item.title === receipt?.title);
        const others = s.catalog.filter(item => item.title !== receipt?.title);
        const items = [...matches, ...others].slice(0, 50);
        for (const item of items) {
          const row = el('li'); row.append(el('span', item.title || 'Untitled playlist'), el('code', item.id)); catalogItems.append(row);
        }
        catalogSummary.textContent = s.catalog.length
          ? s.catalog.length.toLocaleString() + ' saved ' + (s.catalog.length === 1 ? 'playlist.' : 'playlists.')
            + (matches.length ? ' ' + matches.length + ' ' + (matches.length === 1 ? 'entry has' : 'entries have') + ' the requested name; names can repeat.' : ' No entry has the requested name.')
            + (s.catalog.length > items.length ? ' Showing the first 50, with matching names first.' : '')
          : 'No saved playlists were returned. An unconfirmed save can still complete later.';
      }
      if (s.pending && s.pending !== shownReview) { reviewFocus = doc.activeElement; preview.focus({ preventScroll: true }); }
      else if (!s.pending && shownReview && s.open && !s.writing && reviewFocus?.isConnected && !reviewFocus.disabled) reviewFocus.focus({ preventScroll: true });
      shownReview = s.pending;
      if (receipt && receipt !== shownResult && s.open && !s.writing) result.focus({ preventScroll: true });
      if (!s.writing) shownResult = receipt;
      try { changed(s); } catch (_) { /* Presentation only. */ }
    }
    control = S.model({ call, controller, saved, now, changed: render });
    async function browseSaved() {
      const receipt = control.state.result;
      if (browseButton.disabled || receipt?.status !== 'accepted') return false;
      browsing = true; control.close();
      let answer = false;
      try { answer = await browse(receipt); }
      catch (_) { answer = 'The saved catalog could not be opened. Use Link or Sonos playlist to browse it.'; }
      finally { browsing = false; }
      if (disposed) return false;
      if (answer !== true && !doc.hidden && controller.state.active && !doc.querySelector('dialog[open]')) {
        viewMessage = typeof answer === 'string' ? answer : 'Select the original listening group, then open Link or Sonos playlist to browse its saved catalog.';
        control.open();
      }
      render(control.state); return answer === true;
    }
    const open = () => {
      opener = doc.activeElement; viewMessage = '';
      if (!control.open()) return false;
      if (canReadQueue()) void readQueue();
      if (!name.disabled) name.focus();
      return true;
    };
    on(trigger, 'click', open);
    on(name, 'input', () => { viewMessage = ''; if (!control.edit('title', name.value)) render(control.state); });
    on(form, 'submit', event => { event.preventDefault(); viewMessage = ''; void control.review(); });
    on(dialog, 'cancel', event => { event.preventDefault(); control.close(); });
    on(dialog, 'close', () => { if (control.state.open) control.close(); });
    render(control.state);
    return { control, open, observe: control.observe, suspend: control.suspend, resume: control.resume,
      dispose() {
        if (disposed) return;
        control.dispose(); disposed = true;
        for (const [node, event, fn] of listeners) node.removeEventListener(event, fn);
        if (dialog.open) dialog.close();
      } };
  }
  return { panel };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = DashboardPlaylistSavePanel;
