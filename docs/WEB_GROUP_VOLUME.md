# Whole-group volume in the web remote

Every card containing two or more rooms includes **Group volume**. Move the
slider to choose a level, then confirm **Set group to …**. The gesture alone
is a local draft. The panel reads the coordinator's `group_volume`, never the
coordinator's own room level or an invented average of the room sliders.

Confirmation checks the same household, coordinator, membership and group
level again, then sends one native `POST /volume` with `group: true`. Existing
policy caps, quiet hours, action history and native undo remain authoritative.
No Play, Resume, Mute or source replacement is sent. The browser does not
simulate a group change with a loop of individual volume requests.

An acknowledged level and its policy notes are shown separately from the
next observed level. A cap does not cause an automatic ramp toward the
requested value. Another controller can change the group after preflight;
this is not an atomic speaker reservation or cross-client transaction.

The panel acquires the existing native playback-action lane and the member
room volume lanes. An outstanding room slider write or unsent debounced
adjustment prevents group dispatch. Replacement cards inherit outstanding
lanes, and completion fences old volume reads before shared reconciliation.
There is no additional polling timer or event connection.

A lost or malformed write response is never repeated. Its outcome remains
uncertain across hiding, coordinator replacement, splits and merges. Passive
updates can display current state but cannot clear this condition. A successful
explicit **Refresh group level** clears uncertainty only for that group's
members, not absent rooms, sibling groups or still-pending commands. Records
are local to this loaded browser page, not persisted across a page reload.
Individual room controls and independently mounted controls remain separate
user actions; these records do not create a durable daemon command service.

## Checks

```sh
node --test scripts/test-remote-group-volume.cjs
```

The tests execute the shipped model, panel and native-lane adapter with
synthetic fixtures, including real loopback HTTP success and lost-response
cases. They are not Rust-daemon, GENA or physical-speaker acceptance tests.
