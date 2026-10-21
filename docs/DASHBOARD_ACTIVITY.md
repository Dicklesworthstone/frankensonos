# Listening Room: activity and checked undo

Open **Activity & undo** near the bottom of `/dashboard`. Read the newest 20
or 100 actions, expand an entry to see its caller, surface, policy decision and
reported result, and filter the loaded window locally. Times are shown in the
browser's timezone. The window is not the whole log; a failure retains the
last observation with a stale label instead of inventing an empty history.

## Recover a reviewed action

The separate undo preview is selected by the daemon for the current caller.
An action's `undoable` history flag only says that a before-state exists. It
is not current eligibility, and clicking a historical row cannot select an
arbitrary action for restoration.

Choose **Review latest eligible undo**, read the exact action ID, caller and
original result, then confirm. The panel re-reads `/undo/preview` and sends
only `{ "expected_id": ... }` to `/undo/checked`, through the dashboard's
existing single mutation lane. A newer candidate is refused, not substituted.
There is no force option, other-caller override or fallback to `/undo`.

Undo belongs to this caller, not to the currently selected room, and can affect
multiple groups or saved configuration. Other tabs or applications may share
the caller identity. The existing dashboard requires a selected group with
fresh playback state before it permits a mutation; selecting that group does
not narrow the undo's effects.

Current policy, offline speakers and intervening changes can prevent aspects
of restoration. Every returned failure remains visible. An acknowledgement
with no failures means only that no restoration failures were reported, not an
atomic rollback guarantee. There is no redo. Queue deletion/reordering cannot
be reversed here; they retain the daemon's existing non-undoable boundary.

## Recovery behavior

Opening the panel is read-only. Reads are lazy and serialized as one history
and preview pair, with at most one queued refresh. Healthy quiet panels do
not poll. Workspace invalidation retires reviews; obsolete or hidden replies
cannot authorize a command. Server retry minima survive hide/show and repeated
refresh requests; the next event or explicit refresh after that minimum can
read again. No timer silently retries a denied or failed undo.

A command already sent is not cancelled or replayed when the panel closes or
the page hides. Its acknowledgement is retained. Lost or malformed replies
leave the dashboard's shared uncertainty lock in place. Refreshing activity
or receiving background snapshots cannot clear that lock: use the main
**Refresh**, inspect speaker state and history, then deliberately review any
further action. No persistence across a complete browser reload is claimed.

## Checks

```sh
node --test scripts/test-dashboard-activity.cjs scripts/test-dashboard-activity-assets.cjs
python scripts/test-dashboard-activity-browser.py
```

The Node tests execute the production dashboard core and activity model,
including real localhost HTTP success, changed-candidate refusal and lost
acknowledgements. The Chromium script requires Playwright and Chromium
(`CHROMIUM` can select its executable). It executes the production
core/activity/panel/workspace with native DOM, but fixture HTTP, a minimal
host and unrelated queue/library/session collaborators. It covers keyboard
focus, literal metadata, narrow layouts, partial results, hidden races and
shared command exclusion. It is not a full-page daemon-delivery, physical
Sonos, Spotify or Tailscale acceptance run. Rust changes only embed assets;
workspace compilation and hardware qualification remain separate gates.
