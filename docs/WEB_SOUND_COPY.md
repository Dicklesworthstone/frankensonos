# Copy speaker sound from the web remote

Open **Copy speaker sound**, choose a source speaker, and select the destination
speakers explicitly. The source sound is read from the native EQ API. Nothing is
copied by selecting speakers or opening the review. **Review sound copy** reads
every destination and shows each before/after setting; confirm that exact review
to start the copy.

Only bass, treble and loudness are copied. Playback, grouping, volume and mute
are untouched. Speakers are addressed by room and household, including members
that do not coordinate their groups. Matching speakers are skipped, and other
speakers receive only the fields that differ. At most 32 destinations can be
selected; review reads use at most four concurrent requests.

## Results and interruption

This is not atomic. Every selected speaker is checked before the first change,
then each destination is checked again immediately before its turn. Another
controller can still change a physical speaker between a check and a command.
The native API continues to enforce permissions and record undo information.

A command acknowledgement is not a verified observation. Each accepted change
is independently read back before moving to the next destination. A conflicting
change, failed request, malformed response, or failed verification stops the
remaining copy. Results distinguish verified, already matching, conflicting,
uncertain, accepted-but-unverified, and not-attempted speakers. No command is
automatically replayed and no automatic rollback is attempted.

**Stop remaining copies**, closing the panel, or hiding/leaving the page stops
future dispatches. A command already in flight cannot be recalled; its eventual
result is retained. Returning to the page does not resume a copy. Read the source
and make a new review before trying again. Each changed speaker has a separate
action-history entry: one undo does not reverse the entire copy.

## Validation

Run the controller, production-panel event, embedding, and real HTTP regressions:

```sh
node --test scripts/test-remote-sound-copy.cjs
```

The HTTP fixtures use synthetic rooms and a loopback server, not physical
speakers. A Chromium component check at 390px also exercised selection, review,
partial writes, verification, stopping, and uncertain results. Rust workspace
compilation and live Sonos acceptance were not available in the implementation
environment and remain separate checks.
