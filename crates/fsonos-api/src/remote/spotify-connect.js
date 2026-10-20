// The daemon owns sign-in. This page only opens consent and hands back a callback.
'use strict';

const SpotifyConnectRemote = (() => {
  const routes = { spotifyConnectStatus: ['GET', '/auth/spotify'],
    spotifyConnectBegin: ['POST', '/auth/spotify/begin'],
    spotifyConnectComplete: ['POST', '/auth/spotify/complete'] };
  const scopes = Object.freeze(['user-library-read', 'user-follow-read', 'playlist-read-private',
    'user-top-read', 'user-read-recently-played']);
  const MAX_ADDRESS = 16384;
  const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
  class InputError extends Error {}
  const fail = (message) => { throw new InputError(message); };
  // Do not reflect a server error: it could contain the submitted callback.
  function message(error) {
    if (error instanceof InputError) return error.message;
    const hints = {
      NOT_IMPLEMENTED: 'Spotify sign-in is not configured. Set FSONOS_SPOTIFY_CLIENT_ID and the registered redirect on the daemon host, then restart the daemon.',
      POLICY_DENIED: 'The house policy does not let this login use Spotify sign-in. Starting one is the owner\'s: sign in from the daemon host\'s own tailnet login, run fsonos setup on that host, or grant it to this login in policy.toml (grant = ["auth"] under its [clients."<login>"] table).',
      NOT_READY: 'Another Spotify sign-in is being completed. Check status before trying a new sign-in.',
      SPOTIFY_AUTH_REQUIRED: 'Spotify did not accept this sign-in. Check status, then start a new sign-in when ready.',
      INVALID_ARGUMENT: 'No matching sign-in could be completed. Check status before starting a new sign-in.',
    };
    return Object.hasOwn(hints, error?.code) ? hints[error.code]
      : 'The sign-in request did not finish or returned an unreadable answer. Check status on this daemon.';
  }

  function urlOf(value) {
    if (typeof value !== 'string' || !value.length || value.length > MAX_ADDRESS
        || /[\u0000-\u0020\u007f\\#]/.test(value)) fail('The sign-in address is invalid.');
    let url;
    try { url = new URL(value); } catch (_) { fail('The sign-in address is invalid.'); }
    if (!url.href.startsWith(`${url.origin}/`) || url.hash) fail('The sign-in address is invalid.');
    return url;
  }
  function paramsOf(url) {
    const params = url.searchParams;
    if ([...params.keys()].some((key) => params.getAll(key).length !== 1)) fail('The sign-in address contains duplicate fields.');
    return params;
  }
  function loopback(host) {
    const parts = host.split('.').map(Number);
    return host === '[::1]' || (parts.length === 4 && parts[0] === 127
      && parts[1] === 0 && parts[2] === 0 && parts[3] === 1);
  }
  function redirectOf(value) {
    const url = urlOf(value), params = paramsOf(url);
    if ((url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback(url.hostname)))
        || ['code', 'state', 'error', 'error_description'].some((key) => params.has(key))) {
      fail('The daemon redirect must be a secure address or an explicit loopback IP callback. Configure it on the daemon host.');
    }
    return url;
  }
  function statusOf(value) {
    if (!object(value) || typeof value.signed_in !== 'boolean' || typeof value.pending !== 'boolean') {
      fail('The sign-in status is unreadable. Check status again.');
    }
    redirectOf(value.redirect_uri);
    return Object.freeze({ signed_in: value.signed_in, pending: value.pending, redirect_uri: value.redirect_uri });
  }
  function consentOf(value, redirect) {
    if (!object(value) || value.redirect_uri !== redirect) fail('The daemon redirect changed. Read status and start again.');
    redirectOf(redirect);
    const url = urlOf(value.url), p = paramsOf(url);
    const fields = ['client_id', 'response_type', 'redirect_uri', 'code_challenge_method', 'code_challenge', 'scope', 'state'];
    if (url.protocol !== 'https:' || url.hostname !== 'accounts.spotify.com' || url.port || url.pathname.slice(1) !== 'authorize'
        || [...p.keys()].some((key) => !fields.includes(key)) || fields.some((key) => !p.get(key))
        || p.get('response_type') !== 'code' || p.get('redirect_uri') !== redirect
        || p.get('code_challenge_method') !== 'S256' || !/^[A-Za-z0-9_-]{43}$/.test(p.get('code_challenge'))
        || !/^[A-Za-z0-9]+$/.test(p.get('client_id')) || !/^[A-Za-z0-9_-]{1,1024}$/.test(p.get('state'))) {
      fail('The daemon did not return a valid Spotify PKCE consent link. Nothing was opened.');
    }
    const requested = p.get('scope').split(/\s+/);
    if (!requested.includes('user-library-read') || requested.some((s) => !scopes.includes(s))) {
      fail('The consent link requests permissions outside read-only music taste. Nothing was opened.');
    }
    return Object.freeze({ url: url.href, redirect, state: p.get('state'), scopes: Object.freeze(requested) });
  }
  function callbackOf(value, redirect, expectedState = null) {
    if (typeof value !== 'string') fail('Paste the full callback address from the Spotify tab.');
    const url = urlOf(value.trim()), base = redirectOf(redirect), p = paramsOf(url);
    if (url.origin !== base.origin || url.pathname !== base.pathname
        || [...base.searchParams].some(([key, v]) => p.get(key) !== v)
        || [...p.keys()].some((key) => !base.searchParams.has(key)
          && !['code', 'state', 'error', 'error_description'].includes(key))) {
      fail('That address does not match this daemon’s configured callback. Nothing was sent.');
    }
    const state = p.get('state');
    if (!state || state.length > 1024 || (expectedState !== null && state !== expectedState)) {
      fail('The callback does not match this sign-in. Use the latest Spotify tab or check daemon status.');
    }
    if (p.has('error')) {
      if (p.has('code')) fail('The callback contains contradictory results. Nothing was sent.');
      fail('Spotify access was declined or failed. No callback was submitted; start a new sign-in when ready.');
    }
    const code = p.get('code');
    if (!code || /[\u0000-\u001f\u007f]/.test(code + state)) fail('The callback has no usable code and state. Nothing was sent.');
    // Forward only the needed fields, to the daemon, never to the pasted address.
    return Object.freeze({ redirect: `?${new URLSearchParams({ code, state })}` });
  }

  function model({ call, changed = () => {}, schedule = setTimeout, cancel = clearTimeout }) {
    let epoch = 0, disposed = false, visible = true, timer = null, reading = false, again = false, failures = 0;
    const state = { open: false, busy: false, loading: false, fresh: false, status: null,
      consent: null, review: null, message: '', uncertain: false };
    const active = () => !disposed && state.open && visible;
    const current = (v) => active() && v === epoch;
    const publish = () => { if (!disposed) { try { changed(state); } catch (_) { /* Never repeat a write after a view failure. */ } } };
    const stop = () => { if (timer !== null) cancel(timer); timer = null; };
    function observe(answer) {
      if (!answer.pending || state.consent?.redirect !== answer.redirect_uri) state.consent = null;
      state.status = answer; state.fresh = true; failures = 0;
    }
    function arm() {
      stop();
      if (!active() || state.busy || reading || (!failures && !state.status?.pending)) return;
      timer = schedule(() => { timer = null; void refresh(); }, failures ? Math.min(60000, 3000 * 2 ** Math.min(failures, 5)) : 3000);
    }
    async function refresh() {
      if (!active()) return false;
      if (reading || state.busy) { again = true; return false; }
      stop(); reading = true; again = false; const v = ++epoch;
      state.loading = true; state.fresh = false; state.review = null; publish();
      try {
        const answer = statusOf(await call('spotifyConnectStatus'));
        if (!current(v)) return false;
        observe(answer);
        if (!state.uncertain) state.message = '';
        return true;
      } catch (error) {
        if (current(v)) { ++failures; state.message = message(error); }
        return false;
      } finally {
        reading = false;
        if (!disposed) {
          state.loading = false; publish();
          if (active() && again && !state.busy) { again = false; void refresh(); } else arm();
        }
      }
    }
    const writable = () => active() && state.fresh && state.status && !reading && !state.busy;
    function reviewBegin() {
      if (!writable()) return null;
      stop();
      state.review = Object.freeze({ status: state.status }); publish(); return state.review;
    }
    function cancelReview() { if (!disposed && !state.busy) { state.review = null; publish(); arm(); } }

    async function write(kind, input) {
      if (!writable()) return false;
      const before = state.status;
      let body = {};
      if (kind === 'begin') {
        if (!input || input !== state.review) return false;
      } else {
        if (!before.pending) { state.message = 'No sign-in is waiting. Check status or start a new sign-in.'; publish(); return false; }
        try { body = callbackOf(input, before.redirect_uri, state.consent?.state); }
        catch (error) { state.message = message(error); publish(); return false; }
      }
      input = null;
      stop(); again = false; const v = ++epoch;
      state.busy = true; state.fresh = false; state.review = null; state.uncertain = false;
      state.message = 'Checking the daemon before sign-in…'; publish();
      let sent = false, acknowledged = false;
      try {
        if (!current(v)) return false;
        const fresh = statusOf(await call('spotifyConnectStatus'));
        if (!current(v)) return false;
        if (fresh.redirect_uri !== before.redirect_uri || fresh.pending !== before.pending || fresh.signed_in !== before.signed_in) {
          observe(fresh);
          fail('Sign-in status changed. Nothing was submitted. Check the current status and review again.');
        }
        state.consent = null; state.message = kind === 'begin' ? 'Starting sign-in…' : 'Completing sign-in on the daemon…'; publish();
        if (!current(v)) return false;
        sent = true;
        const reply = await call(kind === 'begin' ? 'spotifyConnectBegin' : 'spotifyConnectComplete', { body });
        if (kind === 'begin') {
          const consent = consentOf(reply, fresh.redirect_uri);
          acknowledged = true;
          if (current(v)) {
            observe(Object.freeze({ ...fresh, pending: true })); state.consent = consent;
            state.message = 'Open Spotify’s consent page below. Return here after allowing access.';
          } else again = true;
        } else {
          const after = statusOf(reply);
          if (!after.signed_in) fail('The daemon did not confirm a cached sign-in. Check status.');
          acknowledged = true;
          if (current(v)) { observe(after); state.message = 'The daemon reports a cached sign-in. Refresh its library to verify access and load music.'; }
          else again = true;
        }
        return true;
      } catch (error) {
        if (!disposed) {
          state.uncertain = sent; state.message = `${message(error)}${sent ? ' Outcome unconfirmed. This request will not be repeated automatically.' : ''}`;
          if (sent) { state.consent = null; ++failures; }
        }
        return acknowledged;
      } finally {
        body = null; state.busy = false;
        if (!disposed) {
          publish();
          if (active() && (again || (sent && !acknowledged))) { again = false; void refresh(); } else arm();
        }
      }
    }
    function open() { if (disposed) return Promise.resolve(false); state.open = true; publish(); return refresh(); }
    function close() {
      if (disposed) return;
      ++epoch; stop(); again = false; state.open = false; state.fresh = false;
      state.consent = state.review = null; publish();
    }
    function setVisible(value) {
      if (disposed || visible === Boolean(value)) return;
      visible = Boolean(value); ++epoch; stop(); state.fresh = false; state.review = null;
      if (active()) void refresh(); else { again = false; publish(); }
    }
    function dispose() { close(); disposed = true; }
    return { state, open, close, refresh, reviewBegin, cancelReview,
      begin: (review) => write('begin', review), complete: (value) => write('complete', value), setVisible, dispose };
  }

  let nextId = 0;
  function panel({ call, el }) {
    const id = `spotify-connect-${++nextId}`;
    const button = (text, onclick) => el('button', { type: 'button', text, onclick });
    const toggle = button('Spotify setup', () => { if (control.state.open) control.close(); else void control.open(); });
    toggle.setAttribute('aria-controls', id);
    const box = el('div', { class: 'spotify-connect-panel', id, hidden: true });
    const check = button('Check sign-in status', () => { void control.refresh(); });
    const begin = button('Review new sign-in', () => control.reviewBegin());
    const status = el('p', { role: 'status', 'aria-live': 'polite' });
    const facts = el('p'), redirect = el('code', { class: 'spotify-connect-address' });
    const link = el('a', { text: 'Open Spotify consent', target: '_blank', rel: 'noopener noreferrer', referrerpolicy: 'no-referrer', hidden: true });
    const reviewBox = el('div', { class: 'spotify-connect-review', hidden: true, tabindex: -1 });
    const input = el('textarea', { id: `${id}-callback`, rows: 3, maxlength: MAX_ADDRESS, autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false' });
    const complete = button('Finish sign-in on daemon', () => { const value = input.value; input.value = ''; void control.complete(value); });
    const next = button('Continue to library refresh', () => document.dispatchEvent(new CustomEvent('fsonos-library-refresh-open')));
    let reviewed = null;
    const control = model({ call, changed: render });
    function render(s) {
      box.hidden = !s.open; toggle.setAttribute('aria-expanded', String(s.open));
      box.setAttribute('aria-busy', String(s.loading || s.busy));
      check.disabled = s.loading || s.busy;
      begin.disabled = !s.fresh || s.loading || s.busy;
      // A status poll must not steal focus from someone pasting the callback.
      input.disabled = !s.status?.pending || s.busy;
      complete.disabled = !s.fresh || !s.status?.pending || s.loading || s.busy;
      next.disabled = !s.fresh || !s.status?.signed_in || s.busy;
      if (!s.open || s.busy) input.value = '';
      status.textContent = s.loading ? 'Reading sign-in status…' : s.message;
      facts.textContent = !s.status ? 'Sign-in status has not been read.'
        : `${!s.fresh ? 'Last observation (may be stale): ' : ''}${s.status.signed_in ? 'A cached sign-in is available; this is not a live Spotify access check.' : 'No cached sign-in is available.'}${s.status.pending ? ' A sign-in is waiting or being completed.' : ''}`;
      redirect.textContent = s.status?.redirect_uri || 'Read status to see the daemon’s configured callback.';
      link.hidden = !s.consent || !s.fresh || s.busy;
      if (!link.hidden) link.setAttribute('href', s.consent.url); else link.removeAttribute('href');
      reviewBox.hidden = !s.review;
      if (reviewed !== s.review) {
        reviewed = s.review; reviewBox.replaceChildren();
        if (reviewed) {
          const expected = reviewed;
          reviewBox.append(el('p', { text: 'Start a new sign-in for this daemon. This replaces any pending consent link, but does not erase the cached sign-in or start music. Spotify will ask for read-only library and taste access.' }),
            button('Start this sign-in', () => { void control.begin(expected); }), button('Cancel', () => control.cancelReview()));
          reviewBox.focus({ preventScroll: true });
        }
      }
    }
    box.append(el('h2', { text: 'Connect your Spotify library' }),
      el('p', { text: 'The daemon handles sign-in and stores access locally. Your Sonos account linkage is separate: Spotify must also be linked to each household in its Sonos app.' }),
      el('div', { class: 'row' }, check, begin, button('Close', () => { control.close(); toggle.focus(); })), status, facts, reviewBox,
      el('p', { text: 'Configured callback (must exactly match the app’s registered redirect):' }), redirect,
      el('p', {}, link),
      el('details', {}, el('summary', { text: 'Finish from a phone or another computer' }),
        el('p', { text: 'A loopback callback on another device can show a connection error. Copy the full address from that Spotify tab after consent, return here and paste it below. The address is sent only to this daemon, never opened or fetched by this panel. Keep it private; do not paste it into chat or logs.' }),
        el('label', { for: input.id, text: 'Full callback address from the Spotify tab' }), input, complete),
      el('p', { text: 'Return here and check status after consent. Sign-in alone does not load music; continue to library refresh, then explicitly request a refresh.' }), next);
    const visibility = () => { if (document.hidden) input.value = ''; control.setVisible(!document.hidden); };
    document.addEventListener('visibilitychange', visibility); visibility(); render(control.state);
    return { node: el('div', { class: 'spotify-connect' }, toggle, box), model: control,
      dispose() { input.value = ''; document.removeEventListener('visibilitychange', visibility); control.dispose(); } };
  }
  return { routes, scopes, MAX_ADDRESS, statusOf, consentOf, callbackOf, model, panel };
})();

if (typeof module !== 'undefined' && module.exports) module.exports = SpotifyConnectRemote;
if (typeof document !== 'undefined' && typeof ROUTES !== 'undefined') {
  Object.assign(ROUTES, SpotifyConnectRemote.routes);
  const spotifyConnectRoot = document.getElementById('music-library');
  if (spotifyConnectRoot) spotifyConnectRoot.append(SpotifyConnectRemote.panel({ call, el }).node);
}
