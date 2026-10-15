# Exact scene groups and confirmed transitions

A scene's `groups` describes exact membership, not an instruction to add rooms
while retaining every existing member. A declared coordinator with no `members`
becomes a solo room. A subset keeps its listed members and detaches the other
rooms currently in that group. Rooms assigned to another scene group move once.
Groups outside the scene stay alone unless one of their rooms is explicitly
moved. A scene containing only volume or mute settings does not regroup rooms.
Stereo pairs and home-theater sets remain logical rooms; their hidden players
are not independently detached by the planner. Household-qualified scene names
continue to use the contract in `SCENE_TARGETS.md`.

The executor confirms each grouping operation from topology before dependent
operations proceed. It refreshes its local household model between moves, so a
later coordinator handoff does not use the original, now-stale group layout.
An acknowledged join that is delayed is given a bounded settle window. A lost
reply is not replayed: a confirmed move advances; an unconfirmed move fails.
Topology witnesses that fail are not repeatedly polled during the transition.
Each read retains the transport's own timeout in addition to the settle budget.

Original groups connected by requested joins form a dependency component. A
failed or unconfirmed move blocks later group changes and sound changes in that
component, while independent groups and households continue. Before starting
music, the observed audience must exactly match the intended room set. An
unexpected added room therefore prevents the source change even when the
requested join succeeded. Failed components are excluded from fade preparation;
they are not silently muted in anticipation of a group that never formed.
A failed source also prevents a following play or pause from acting on the old
source. Reports identify the failed and withheld steps rather than calling the
whole scene successful.

The existing shared surface still owns policy, before-state capture, action
logging, undo, and schedules. The before-state includes rooms detached from a
declared coordinator, so undo can restore the previous group. Reapplying an
already-matching scene remains a no-op. These are bounded observational checks,
not a transaction over speakers or a lock against another controller. A client
can change topology after the last observation. Inspect partial reports before
retrying; an unconfirmed network operation may still have taken effect.

## Verification

```sh
cargo test --locked -p fsonos-core --lib scenes
cargo test --locked -p fsonos-api --test scenes_layout_sim
cargo test --locked -p fsonos-api --test scenes_sim --test scenes_households_sim
```

The core planner includes an exhaustive four-room model (4,264 layout/partial
scene combinations). The shared-surface socket tests cover exact solo/subset
membership, a stereo pair, independent households, undo, delayed and ignored
joins, lost replies, failed witnesses, failed-source dependencies, and concurrent
extra membership. They use only synthetic local simulator devices, not the
owner's speakers. Executing these Rust suites and physical-device acceptance
are separate validation steps; independent model checks are not substitutes.
