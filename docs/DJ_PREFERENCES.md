# Standing DJ preferences

Standing preferences apply to every group's future DJ selections. They live in
`preferences.toml` beside `moods.toml` and can be edited through the CLI, HTTP,
MCP, the phone remote, or the Listening Studio's **Standing taste** panel.

The existing preference lists favor or avoid genres, artists, eras and moods;
pin or ban specific artists, albums and tracks; set a default energy; and choose
whether explicit tracks may play. An active room's steering has higher
precedence than standing taste. Learned feedback and account signals cannot
outweigh standing restrictions.

## Choose what happens when every candidate is avoided

By default, the DJ relaxes standing avoids when they exclude every remaining
playable work. Its pick explanation identifies that fallback. The new
`strict_avoids` preference lets the owner keep those avoids in force instead:

```sh
fsonos dj prefs set avoid.genres jazz
fsonos dj prefs set strict_avoids true
fsonos dj prefs show
```

With strict avoids enabled, an exhausted pool produces an explained
`DJ_POOL_EMPTY` instead of selecting avoided music. This applies to genre, artist,
era and mood avoids, across every room and future selection. A small permitted
pool can still rotate normally. The explanation counts effective banned,
explicit and avoided works separately, for example:

> your preferences keep all 3 works out: 1 banned, 1 explicit while explicit tracks are off, 1 avoided while strict avoids are on

Already queued music and current playback continue. Changing this preference
does not clear a queue, stop radio, change volume or replace an existing DJ
session. A failed new start or rehearsal does not publish a hypothetical pick.
Edit the avoided categories or the permitted library when the DJ has nothing
left to choose.

Pins still outrank avoids. Steering that explicitly names an artist, composer
or genre retains its existing precedence over standing taste. Explicit-content
restrictions and temporarily unavailable tracks keep their existing rules.
`strict_avoids` controls only the exhausted-avoid fallback; it is separate from
a room direction's `--strict` filter option. Either option can be used alone or
together.

To restore adaptive fallback while retaining every avoidance entry:

```sh
fsonos dj prefs set strict_avoids false
# Equivalently, reset just this scalar setting:
fsonos dj prefs unset strict_avoids
```

Existing preference files default to `strict_avoids = false`. The value must be
`true` or `false`; an invalid edit does not replace the accepted file. Preference
edits use the existing serialized file update and apply from the next pick.
They are not playback undo actions; use set or unset to change them again.

## Phone remote and Listening Studio

Open the standing preferences and find **When all permitted music is avoided**.
Choose **Relax avoids when needed** or **Keep avoids and wait**, then **Review
avoid setting**. Review explains that the setting applies to every DJ and that
pins, explicitly named steering and queued music retain their behavior.
Confirm with **Save for every DJ**.

The editors retain an uncertain result if a save reply is lost, malformed or
contradicts the reviewed strict setting. Read preferences again before deciding
on another edit. They never replay a submitted save automatically. An absent
field in an older daemon's read is shown as the existing relaxed default;
enabling strict avoids requires a reply confirming `strict_avoids: true`.

## HTTP and MCP

Read `GET /dj/preferences` or call `dj_preferences`. The response includes the
boolean `strict_avoids` alongside `energy`, `explicit`, `favor`, `avoid`, `pin`
and `ban`.

Use the existing `POST /dj/preferences` mutation to enable the option:

```json
{"key":"strict_avoids","value":"true"}
```

Use `{"key":"strict_avoids","unset":true}` to restore its default. MCP's
`dj_prefer` takes the same `key`, string `value` and optional `unset` fields.
The mutation response retains `done`, `changed` and the complete accepted
`preferences`. Existing preference permissions and action logging apply.

## Focused regression checks

The engine cases cover all four avoid families, legacy defaults, persistence,
pins, named and expired steering, temporary unavailable tracks, seeded rotation
and exact exclusion reasons. CLI simulator coverage checks readiness, rehearsal
and start against an exhausted strict pool while retaining current playback.

```sh
cargo test --locked -p fsonos-spotify --lib strict_avoids
cargo test --locked -p fsonos-api --lib surface::dj_prefs::tests
cargo test --locked -p fsonos-cli --bin fsonos strict_avoids
cargo test --locked -p fsonos-mcp --lib strict_standing_avoids
node --test scripts/test-remote-dj-preferences.cjs scripts/test-dashboard-preferences.cjs
```
