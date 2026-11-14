# Save and manage Sonos playlists in the Apple remote

The native iPhone, iPad and Mac remote can save the current group's queue,
rename an existing Sonos playlist, or delete it from its original household.
These are playlists stored on Sonos, separate from the Spotify account's
playlists. No Spotify Web API sign-in is needed.

## Save a queue

Open **Music → Up next → Save queue as playlist**, enter a title, then choose
**Review save**. The review reads the group's current retained queue and shows
its entry count, first entry, audible source and any existing playlists with
the same title. Confirm **Save new playlist** to send the change once.

Saving retains the queue even when radio or another source is playing. It does
not interrupt playback, alter volume, or stop DJ feeding. An equal title creates
another playlist with its own native `SQ:` ID; it never overwrites the existing
entry. Titles are trimmed, must be nonempty, cannot contain control characters,
and may use at most 1023 UTF-8 bytes.

The queue revision and group are checked again on confirmation. The daemon's
save route does not accept a client queue revision: Sonos saves the queue when
it handles the request, so a change from another controller after the last
check can still be included. The review is an observation, not a reservation.

## Rename or delete an exact saved playlist

Open **Music → Sonos**, then use the playlist's options menu for **Rename saved
playlist** or **Delete saved playlist**. The same menu is available while
reading its tracks. Review identifies the original household, native playlist
ID, title and current entry count. Confirmation reads the catalog and contents
again, then sends the exact `SQ:` ID and the reviewed full-contents revision.
A changed title, contents revision or group requires a fresh review.

Renaming preserves saved tracks. Deletion permanently removes the saved catalog
entry. Both leave current playback and the group's retained queue unchanged.
Neither catalog change can be undone. A scene pinned to a renamed playlist's
old title must be saved again; a scene referencing a deleted playlist can no
longer load that source. An empty saved playlist can still be removed.

## Results and uncertain replies

The result keeps the original household and exact native identity even after
changing rooms, closing the sheet or backgrounding the app. A confirmed change
whose separate action-log write failed still counts as confirmed; the message
explicitly says not to repeat it.

If the connection is lost, cancellation happens after submission, or the reply
cannot confirm the requested operation, the result stays **uncertain**. The
remote never automatically submits it again. Choose **Inspect playlist result →
Check original household's saved playlists** to read the original household's
catalog. This follows the original room through regrouping and refuses to
substitute a same-named room in another household. If the original room is no
longer available, recovery waits for it to become addressable.

A matching title, renamed entry or missing ID describes the present catalog;
it does not prove which earlier request caused that state. A successful explicit
read permits a new, separately reviewed catalog change while retaining the
earlier uncertain receipt. Closing or reopening a panel cannot clear that
requirement. Playback controls are available once the outstanding request drains.

All three actions share the native remote's existing command-review lane with
playback, scenes, schedules, preferences and undo. A review is consumed before
confirmation performs any asynchronous read, so repeated confirmation cannot
send duplicate changes. Stopping an unsent review prevents submission; stopping
an already submitted request does not reverse its effect.

## Validation

`NativePlaylistManagementTests` adds 18 test declarations, expanding to 26
parameterized cases. Its synthetic transport runs the production review,
client and workflow code for retained radio queues, duplicate titles, both
households, stale queue/content revisions, changed membership, empty playlists,
shared command ownership, cancellation, inconsistent and lost replies, failed
log writes, original-household recovery, and late results after suspension.

Run the focused package tests with:

```sh
swift test --package-path apps/apple --filter NativePlaylistManagementTests \
  -Xswiftc -warnings-as-errors
```

The implementation environment had no Swift toolchain. These tests were added
and statically reviewed but were not executed locally; Swift compilation,
Apple SDK type checking and rendered iPhone/iPad/Mac interaction remain separate
verification steps. The new SwiftUI view is registered in the checked-in iOS
project as well as the Swift package's automatic source discovery. No real
speaker or account was used during implementation.
