# Native playlist selections as Apple scenes

The iPhone, iPad and Mac app can turn selected native Sonos playlist occurrences
into a reusable scene, including local files and native service tracks. Saving
does not change any speaker's playback, queue, grouping or volume.

## Create a scene

1. Open **Music → Sonos playlists**, then open a saved playlist.
2. Add up to ten occurrences to **Selected tracks**. Paging retains the same
   playlist revision. Move, repeat or remove entries to choose the exact order.
3. Choose the destination using **Send selection to**. A displayed room, a saved
   alias or every group in the original household can supply the scene groups.
4. Choose **Create scene from selection**. The app reads the retained source
   pages and resolves the destination into the explicit groups shown in the
   scene composer. A later alias edit does not redirect those groups.
5. Give the scene a name and inspect each group. You can choose a different
   leading room and members within the source household, add room levels or
   mutes, and choose **Play** or **Prepare silently** for each native selection.
6. Choose **Review custom scene**, inspect the exact ordered tracks and every
   affected room, then confirm the save. Replacing an existing scene shows the
   previous definition and retains its observed storage revision.

The saved scene retains the selected resources, full native playback metadata,
order and repetitions. Later playlist edits, renaming or removal do not change
that sequence. It can be recalled from **House → Scenes** or used in an ordinary
**Apply scene** routine. Playback still uses the original household's native
service linkage or local file access.

## Review and identity

The app keeps the original source room, playlist id, contents revision and
each selected page. It rereads all retained pages before review and again before
saving, including occurrences from earlier pages. A changed revision, resource,
title, source group or reviewed destination group requires another review.

The write supplies only the source room, native playlist id, ordered positions
and expected contents revision. Renderer metadata is captured by the daemon;
the app cannot submit authored DIDL. The returned saved scene must confirm the
native household, playlist identity, exact ordered resources and titles, group
membership and playback intent. Lost or contradictory acknowledgements remain
uncertain and are never retried automatically.

The native selection is bound to its original household. The composer refuses
a destination in another household. Saving a scene freezes the selection;
copying a saved scene's public display fields into a new definition does not
provide the source evidence required to capture it again.

**Prepare silently** means that applying the scene later loads its selected
queue without starting playback. **Play** starts its first occurrence in the
saved order. Both use the existing daemon scene execution, policy, schedules
and bounded undo behavior described in [Native track scenes](NATIVE_TRACK_SCENES.md).

## Focused verification

```sh
swift test --package-path apps/apple --filter NativeTrackSceneTests
swift test --package-path apps/apple --filter nativeSceneManagement
swift test --package-path apps/apple --filter SonosPlaylistTracksTests
```

The tests exercise the production authoring, conditional-save, review, receipt
and screen model paths using a daemon transport fixture. They cover S1/S2,
multiple source pages, repeats, silent scenes, destination aliases, stale source
and group evidence, cancellation, conflicts and uncertain acknowledgements.
