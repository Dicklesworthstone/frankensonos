# The listening studio

The full remote at `/` is organized around listening, not around API modules.
The separate immersive Listening Room remains at `/dashboard`.

## Listen first

Choose a group from the room rail. The selected group brings its actual native
player, queue, per-room levels, reviewed group volume, and DJ controls forward.
Same-name rooms remain separate by household. A removed group cannot leave an
invisible stale card selected; the rail follows current topology.

Album art still comes from the daemon's same-origin art endpoint. When art is
unavailable, an explicitly labelled record-sleeve graphic is the fallback. The
studio does not fetch artwork, fonts, or scripts from a third-party origin.
Dark and light appearances use local system fonts and a saved local appearance
choice. The mobile layout has a bottom workspace bar and safe-area spacing.

The **Chosen for you** section uses the existing DJ status contract. It shows
the selected group's current work and its reason summary, the reported movement
within a complete work, any constraints that were relaxed to make a pick, the
steering source, and up to three planned works. Plans are explicitly distinguished
from playback. A stopped DJ, missing work, failed read, and stale observation have
separate states; the page invents no selection explanations.

This is one scoped read lane, not another event stream or periodic poll. Group
selection, returning to Listen, explicit Refresh DJ, native DJ change events,
and selected-group invalidation followed by a native observation can request a
read. Concurrent requests coalesce, stale replies cannot overwrite a new group,
and a failed refresh retains the original observation time. The request uses
the native bounded read client and the existing `DjSessionRemote` decoder.
Hidden pages and other workspaces stop starting DJ-story reads. An already
started request can finish, but obsolete results cannot repaint the story.

**Shape what's next** opens the native steering editor for the selected group,
using its existing read-only `fsonos:dj-edit` navigation event. The native editor
rechecks membership before prefilling the selection. It still requires a new
review and explicit save; opening an editor cannot change playback or steering.

## Find the project's distinctive capabilities

**Music & DJ** brings library browsing, direct source playback, session reasoning,
whole-work steering, and standing taste preferences together. **House & sound**
contains grouping, playback handoff, per-speaker tone, sound copying, and play
modes. **Routines** groups scenes, sleep timers, native fades, schedules, and
announcements. **Activity** makes native action history and supported undo
reachable without competing with the player.

The feature finder opens with the button or Command/Ctrl K. Search descriptions
and feature words such as `whole work`, `move music`, or `bedtime`; arrow keys
navigate the results and Escape closes the dialog. Results come only from mounted
native panels, and unknown future native panels remain reachable in House.

## Same commands, same safeguards

The presentation moves native section roots and decorates existing card nodes;
it does not copy their contents, recreate their event handlers, or introduce a
second speaker-command implementation. Original form drafts, pending reviews,
request outcomes, queue revisions, ownership exclusions, policy caps, and
recovery controls remain owned by the native modules. Workspace navigation and
room selection do not dispatch speaker writes. Choosing a room in the rail does
not silently retarget a review in a different native tool.

The new `/studio.js` and `/studio.css` assets use the existing hardened asset
helper and content security policy. No credentials or site-specific identifiers
are embedded. Synthetic browser data lives only in the optional test fixture,
which is never included in the daemon.

## Verification

Run the dependency-free Node regressions:

```sh
node --test scripts/test-remote-studio.cjs scripts/test-remote-studio-insights.cjs
```

For optional offline Chromium DOM checks, install Python Playwright and Chromium,
then run `python scripts/studio-check.py`. `FSONOS_STUDIO_CHROMIUM` overrides the
browser executable; `FSONOS_STUDIO_SCREENSHOTS` chooses a screenshot directory.
The script uses the real studio assets with **synthetic native-shaped controls
and API observations** at 320, 390, 768, 960, 1280 and 1440 pixels, in both themes.
It checks control-node identity, original listener dispatch, preserved drafts,
qualified selection, late-mounted panels, scoped DJ reads, failure recovery,
text safety, and overflow. Screenshots label their synthetic fixture data.

These are not full-native-remote, HTTP asset-delivery/CSP, Rust-daemon, real GENA,
or physical Sonos acceptance tests. The full existing JavaScript suite and Rust
workspace remain separate gates. No hardware was contacted for these fixtures.
