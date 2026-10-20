# Native announcements

In the native remote, choose a group, open **House → Announcements**, then select
**Speech** or **Chime**. Speech uses the local engine and default voice configured
on the daemon. Chimes are the daemon's generated `bell`, `beep`, and `rise` clips.

Select the recipient rooms explicitly and choose the requested volume. No rooms
are selected by default. **Select this group's rooms** selects only that group's
current members, not the whole house. A changed household, coordinator or group
membership clears recipients while keeping the typed message. Normal playback
progress and a reordered member list do not clear them.

**Review announcement** does not play audio. The shared native command session
reads the affected group before presenting the exact message/chime, qualified
room names, requested level and interruption/restoration warning. Confirmation
rechecks the observation and consumes the review once. The daemon remains the
authority for access policy, volume caps, synthesis, playback and restoration.
Client reads are not an atomic reservation against another controller.

The result distinguishes clip completion, deadline termination, a clip that never
started, and playback failure. It preserves returned room levels and every
restoration problem. A finished clip with a restoration failure is still partial,
not a clean success. Lost or contradictory replies remain uncertain; no write is
automatically retried. Closing the remote does not recall an accepted announcement.
Announcements restore themselves and are not entries that speaker undo can reverse.

## Implementation boundaries

`NativeCommand.announce(Announcement)` uses the existing `CommandReviewSession`,
`DaemonClient.perform` and `RemoteModel` result path, with a 1,200-second HTTP
request timeout matching the daemon's announcement deadline. Its body contains
exactly one source, a nonempty list of household-qualified rooms, and an explicit
volume (including zero). Empty recipients never reach the API's whole-house default.
Speech is trimmed and checked against the backend's 1,000-Unicode-scalar limit.

This composer addresses a subset of one selected group's rooms. It does not upload
WAVs, select a custom speech voice, combine separate groups, or send a whole-house
broadcast. The existing HTTP, CLI and web announcement interfaces retain their
broader capabilities. No additional endpoint, credential or account scope is used.

## Validation

Run the full package suite with:

```sh
swift test --package-path apps/apple -Xswiftc -warnings-as-errors
```

`AnnouncementTests` and `AnnouncementFormTests` cover request encoding, explicit
recipients, Unicode limits, zero volume, household/group changes, immutable review
values, all completion categories, optional result fields, capped levels and
restoration failures. Eighteen of these tests were executed under Swift 6.2.1 on
Linux in a focused package containing the exact new production files and the
hash-verified upstream `Models.swift`; this is not a full-package test result.
The changed command and the iOS/macOS view branches also passed syntax parsing.
Apple SDK type checking, rendered native UI, full command-session integration,
real speech generation and physical-Sonos acceptance still require their own checks.
