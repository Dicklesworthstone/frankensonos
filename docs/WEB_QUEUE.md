# Queue management from the web remote

Open the daemon's existing web remote on loopback or through Tailscale Serve.
Every room card has a **Queue** section. The browser uses the same public JSON
API, caller identity, house policy and action log as the other control surfaces;
it never talks directly to a speaker or to Spotify.

## Browse and play

Opening **Queue** puts the entries first, with the visible position range,
current-entry shortcut, and refresh control above the list. **Find in queue**
and **Add music** expand when needed. **Position & range tools**, below the
list, contains numeric navigation, range selection, and precise move placement.
The controls retain their drafts and expanded state through a queue redraw.

Search results remain separate from queue order. Choosing **Show position**
opens the exact occurrence and collapses the search controls so the entry gets
focus; the collapsed search still reports its match count. Selections, including
entries on other pages, stay visible above the list. Error messages and move or
removal confirmations always appear outside the collapsible tools.

Opening a queue reads at most 25 entries. Previous/Next page and Refresh queue
read another snapshot; closed sections do not poll. The current queue position
is marked, and every entry shows its title and available artist/album metadata.
Pages are replaced rather than appended, so items from different revisions are
never combined into a single selection.

**Play** selects an existing entry without re-enqueueing it. The browser sends
both its exact renderer URI and the opaque revision returned with that page to
`POST /queue/play`. The server retains coordinator resolution, volume caps,
quiet hours and its playback undo behavior. A changed selection is refused,
not silently redirected to a different track at the same position.

## Add music

Expand **Add music** and paste a Spotify track, album, playlist, artist, episode
or show URI/link.
**Add to end** and **Queue next** call `POST /queue`; Spotify URI normalization and
validation remain in the shared server action. These buttons add entries, not
start playback. If the group is on a radio or another source, use an entry's
Play button to activate the queue. The server's result explains where the
addition went and whether the group is playing its queue.

## Remove a range

Select one entry, or up to 100 consecutive entries across pages, then
choose **Review removal**. The review lists the exact titles and first position.
**Cancel removal** sends nothing. **Remove permanently** calls
`POST /queue/remove` once with every selected renderer URI, in order, and that
page's revision. Repeated URIs remain repeated entries; titles are display-only.

**Removal cannot be undone.** Removing the current entry may advance or stop
playback. Removing an entry does not ban it from future DJ picks or stop the DJ.
Under **Position & range tools**, use **Read and select range** to capture a
longer block by its first and last positions, or keep selecting while browsing
adjacent pages of the same queue revision. A refresh, page change, edited
selection or rebuilt room card
invalidates the prior confirmation.

## Reorder without calculating positions

An entry's **Move…** menu offers earlier, later, after the current entry, start,
and end destinations. It always identifies that one occurrence, even if you
have other entries selected. The selection toolbar offers the same actions
for moving a contiguous block together. Touch and keyboard use the same
controls; Escape closes a row's menu and cancellation returns focus.

Every choice opens the existing move review. Only **Move entries** changes
queue order, with the exact renderer URIs and revision. Moving does not start
playback. Unavailable destinations are disabled; arbitrary numeric placement
remains available. See [queue movement](QUEUE_MOVES.md) for destination and
conflict behavior.

## Conflicts and uncertainty

The browser does not parse or manufacture queue revisions. A daemon which omits
them remains browsable but is read-only in this panel. These revisions are
observations, not locks or persistent device identities across a reboot.

While a write is pending, the panel cannot send another write. Failure or an
unreadable acknowledgement discards the selection and requires an explicit
Refresh queue before another attempt. There is no automatic write retry or
fallback to an unguarded endpoint. After an acknowledged success, only a read
refresh runs automatically. Deleting the last entries on a page returns to a
remaining page. Metadata is inserted as text, never as markup or a link.

## Checks

`node --test scripts/test-remote-queue.cjs` executes the shipped controller code
using Node's built-in test runner, without installing JavaScript packages. The
suite covers paging, exact identities/revisions, stale asynchronous reads,
disposal, single-dispatch writes, malformed responses, review/cancel behavior,
contiguous removal, duplicate URIs, and Spotify queue additions. The Linux
workspace workflow runs it before the Rust gates.

Chromium DOM checks with mocked API responses cover phone/desktop rendering,
keyboard focus, read-play-conflict, review/cancel/remove, and Spotify queueing.
These checks do not prove Rust HTTP integration or physical-Sonos behavior;
the workspace/simulator and hardware checks remain separate gates.

The optional `scripts/test-remote-queue-reorder-browser.cjs` check covers the
shipped queue UI at 320, 390, 768, and 1440 pixels in both themes, including
search, paging, off-page selections, disclosure state, retained drafts, keyboard
focus, review and visible error states. Set `FSONOS_QUEUE_CHROMIUM` to a Chromium
executable and make Playwright available to run it. Ordinary Node test runs skip
this browser-only check when the executable is not supplied.
