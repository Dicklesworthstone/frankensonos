# Scenes across households

Scene definitions use the same exact room names, household qualifiers and
player IDs as direct control. This applies to group coordinators and members,
volume and mute keys, favorite selection, captured house states, and scheduled
`apply_scene` actions. Prefixes, aliases and reserved sets are not expanded in
a persistent scene: a missing room must not silently become another target.

For example, two households can each have a Kitchen and an Office:

```json
{
  "groups": [
    {"coordinator": "Kitchen@S1", "members": ["Office@S1"]},
    {"coordinator": "Kitchen@S2", "members": ["Office@S2"]}
  ],
  "volumes": {"Kitchen@S1": 20, "Kitchen@S2": 30},
  "mutes": {"Office@S1": true, "Office@S2": false}
}
```

Use this definition with `PUT /scenes/<name>`, MCP `save_scene`, or the CLI's
`scene save --from` input. Saving does not apply it. These remain two separate
groups: S1 and S2 rooms cannot be joined together. Each favorite is looked up
in its selected coordinator's household, even when both households use the
same room name and favorite title.

An unqualified ambiguous name returns `AMBIGUOUS_ROOM` with qualified
suggestions, before the scene can change any speaker. This also applies to
older saved scenes that have become ambiguous. Distinct spellings or IDs of
one physical room cannot put it in two scene groups or silently overwrite
one of its volume/mute settings while saving a definition.

Captured scenes preserve duplicate room names as separate entries, including
stereo-pair rooms. Explicit qualifiers are retained, preferring the household
ID when available; duplicate names within one household use player IDs.
Generated references are checked against the same resolver, including when a
literal room name contains `@`. Discovery-order household indices are never
persisted. Unique unqualified names stay readable for existing scene workflows.

Scenes still refer to named rooms and current scene definitions, not a frozen
future topology. Renamed or absent rooms are skipped with a note. A missing
qualified household never falls back to a same-named room in another household.
Re-save an old scene with explicit qualifiers to remove an ambiguity.

Applying remains policy-bounded, logged, and undoable through the shared
surface. A schedule uses that same path with its creator's rights; a scene's
writer also bounds its volumes. These rules do not add a cross-client lock or
a transaction over physical speakers. A partial application reports failed
steps; inspect its result before deciding what to do next.

## Regression commands

```sh
cargo test --locked -p fsonos-core --lib scenes
cargo test --locked -p fsonos-api --test scenes_households_sim
cargo test --locked -p fsonos-api --test scenes_sim
```

The new socket tests use synthetic S1/S2 households with duplicate room names
and a stereo pair. They cover definition/capture round trips, independent
sources and favorites, restore/undo, ambiguous legacy scenes, member-level
volume, writer caps, and scheduled application. Physical-device acceptance is
separate; these tests do not use the owner's LAN.
