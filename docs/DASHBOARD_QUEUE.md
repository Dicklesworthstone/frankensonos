# Arrange the Listening Room speaker queue

Open `/dashboard`, choose the listening group, then open **Speaker queue**.
This is the actual speaker queue, not the DJ's list of future work selections.
The dashboard reads 40 entries per page and preserves repeated songs as distinct
positions.

**Play** on a row opens the existing confirmation dialog. Confirmation starts
that existing entry, including when paused, without adding tracks or rebuilding
the queue. **Select** opens **Arrange this queue** for that entry. Set the first
and last positions to choose a consecutive range on the visible page. A range
can contain up to 40 entries; select another page for other entries. **Play
selected** is available for a single entry only.

**Move up**, **Move down**, **Move to start**, and **Move to end** prepare a review.
For an exact destination, enter **Move before original position** and choose
**Review move** (or press Enter). Positions refer to the queue before the move;
its original length plus one means the end. For example, moving original entries
3–4 before original position 8 puts them at positions 6–7. Moving inside the
selected range, or just after it, is a no-op and is not submitted. These native
moves do not restart or resume playback. Shuffle may still change the listening
order.

**Remove selected** reviews deletion of exactly that range. Removal and
reordering are not undoable; removing the current entry may interrupt playback.
Manual queue edits can disrupt a multi-movement DJ work. Use the DJ's whole-work
skip instead when that is the intended action. The review shows the group,
positions, selected titles, destination and consequences before any command.

## Identity and recovery

Every actionable selection belongs to one observed page, exact household/group,
revision and ordered renderer URI range. A missing revision or renderer identity
leaves the page readable but not editable. Editing invalid range text retires
the prior selection rather than leaving an older valid range behind. Navigation,
refresh, grouping changes, event invalidation, close and suspension retire old
selections. Retained row callbacks cannot silently select a different page or
household.

The existing dashboard command lane rechecks playback, group membership and the
selected queue page before submitting one native `/queue/play`, `/queue/remove`
or `/queue/move` request. Each includes the queue revision and exact expected
URI or ordered URI range. Moves retain the original destination coordinate.
This is not a cross-browser lock or an atomic reservation of the speaker: the
native backend's revision checks remain authoritative.

Submission retires the old selection immediately. Successful state recovery
then rereads the visible queue rather than assuming a predicted new order.
Operation-specific receipt validation checks the requested group, range and
result. A received acknowledgement remains valid after hiding the view. A lost,
malformed or failed reply remains uncertain in the dashboard's shared command
lane; no automatic write retry occurs. Use the dashboard's explicit **Refresh**,
read the queue and review a new selection before another command. A daemon
read-back recovery is labeled as such, not as a replayed edit.

The design preview remains read-only. The server and preview bundle the same
queue editor and stylesheet; neither gains new endpoints or relaxed browser
security headers.

## Verification

```sh
node --test scripts/test-dashboard-queue.cjs \
  scripts/test-dashboard-queue-actions.cjs \
  scripts/test-dashboard-queue-session.cjs \
  scripts/test-dashboard-session.cjs
python3 scripts/test-dashboard-queue-browser.py
```

The Node command passed all 121 tests, including the unchanged 13 queue and
34 session regressions, with eight real loopback HTTP cases. Four selected
workspace keyboard/bundle-order checks also passed. The two new Node suites
run in the Linux workflow.

The optional browser suite requires Python Playwright and Chromium
(`FSONOS_BROWSER_PATH` may select the executable). All 14 component cases passed
using the shipped queue, core, session and workspace modules with native DOM.
The host, HTTP responses and review presentation are fixtures. Cases cover
keyboard editing, 320/390/768/1440-pixel widths, long metadata, retained targets,
changed revisions, hidden preflight, submitted hidden writes and uncertain
results. This is not a full-page dashboard or live-daemon acceptance test.
Rust formatting, workspace compilation, full-suite, physical Sonos, Spotify
and Tailscale acceptance were not executed in the implementation environment.
