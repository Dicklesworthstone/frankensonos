# Scenes in the native Apple remote

**House → Scenes** captures the current house, authors custom definitions,
reviews replacements and deletion, and applies saved scenes. Browsing and deleting
stored definitions do not require a selected or reachable speaker. Saving requires
the daemon's current room inventory; applying still uses the selected group to
anchor a house-wide review. A scene is never restricted to that selected group.

## Capture and create

**Remember this moment** names a capture of every connected house group. Review
shows the affected household-qualified groups; confirmation asks the daemon to
capture their music, membership, room levels and mutes at that moment. The remote
does not manufacture a scene from its cached playback display. Saving changes
only the scene store and does not send speaker commands.

**Create custom scene** opens a guided draft with optional groups, source and
play/pause settings, room levels, and mutes. Each group has one leading room and
its exact members, all in the same household. A room cannot appear in two groups.
Room levels and mutes can be saved without defining any groups; unlisted settings
remain unchanged when applied. New music sources require an explicit Play or
Pause selection. Empty custom definitions are rejected, because the daemon
interprets an empty PUT body as a request to capture the live house.

Source choices include keeping the current source, a searchable list of playable
Sonos favorites from the chosen household, a complete music URI or Spotify track,
album or playlist link, and the DJ's listed moods or automatic time-of-day program.
Favorite titles must be unique within that household: scenes retain the title,
so selecting an ID does not make a duplicate title safe for later application.
Source reads are bound to the draft group, leading room and source kind; a late
response cannot fill another household's picker. Neither source selection nor
moving a slider sends a write.

**Keep draft** hides the composer while preserving its unsent settings. A
cancelled review, panel switch, playback selection change or suspension also keeps
the draft. **Continue your draft** returns to it. Creating another custom scene
or choosing an explicit custom replacement starts a new definition.

## Replacement and deletion

Entering an existing name or using a scene's **Manage** menu produces an explicit
replacement review, including the previous definition. Scene names follow the
daemon's ASCII-only case equivalence and its 64-Unicode-scalar length limit.
Case-only replacement preserves the requested new spelling; it does not confuse
distinct Unicode names. The complete previously observed row is reread before
review and confirmation. A scene added, changed or removed by another controller
requires a fresh review.

**Replace with a custom definition** deliberately starts with blank settings.
The scene API omits opaque renderer metadata present in some captured sources,
so turning an arbitrary saved DTO back into an editable PUT body would lose data.
This workflow is explicit new authoring, not a lossless editor for captured scenes.
The stored definition remains untouched until replacement is confirmed. Capturing
the house again preserves the daemon's own metadata handling.

**Review deletion** shows the exact stored definition and sends one DELETE only
after confirmation. It needs no room reads. Replacement and deletion are store
changes and cannot be undone from Activity. They do not change current playback;
schedules referring to the scene name will use the replacement or need another
scene after deletion.

## Apply and review guarantees

Expand a saved scene to inspect every coordinator, member, source, playback
setting, level and mute. **Review scene** reads the house and saved definition;
**Confirm once** reads them again before one `POST /scenes/{encoded-name}/apply`.
The optional **Transition** control requests a 1–60 second fade for scene levels.
The daemon remains authoritative for policy, fade support and partial outcomes.
Results preserve every failed step, skipped room and policy note.

Scene capture, custom save, deletion, playback, schedules and Undo share one
`CommandReviewSession` gate. Only one review is current, and confirmation consumes
it before awaiting more I/O. Explicit definitions check room topology, favorite
identity/content and DJ mood constraints again before PUT. A successful save
receipt must match every authored field; the daemon's additional favorite URI
is the one allowed enrichment. Capture receipts describe the daemon's actual
capture, rather than an assumed local snapshot.

These checks are not atomic reservations: the scene API has no conditional-write
token, so another controller can edit between preflight and mutation. Lost,
cancelled or malformed write acknowledgements remain uncertain and are never
retried. Late receipts stay attributed to the original scene across navigation
and suspension. Failed list reads remove stale actionable rows. Reviews scroll
independently of their confirmation buttons.

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Production codec, actor, transport and screen-model tests cover authoring,
capture, exact payloads, source drift, replacements, deletion without rooms,
fade requests, cancellation, uncertain receipts and preserved drafts. Linux
execution does not validate SwiftUI; Apple SDK builds and physical-device
acceptance cover the platform boundaries separately.
