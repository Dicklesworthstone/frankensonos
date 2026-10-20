# Control named room sets and households

In the web studio, open **House & sound → Control a room set or household**.
The control finder also finds it under "alias", "here", "everywhere", or
"collective volume". Enter an existing alias, a room name, `here`, `everywhere`,
or a household-qualified target such as `all@S1`. Reading suggestions does not
select a target or send a command. Aliases and defaults are configured through
the existing CLI/local `aliases.toml` workflow; this screen does not edit them.

Choose pause, resume, mute, unmute, a volume increase/decrease, or an absolute
volume. **Resolve and review** shows the target's current meaning before the
confirmation button becomes available. Resume also reads the source and
transport state of each affected group before review and again before sending.
An unknown or changed source requires another review. Merely advancing within
the same track does not invalidate it.

## Room-level and group-level scope

Pause and resume act once per complete group. If an alias names only one
member of a group, the review explicitly lists the other members that will be
affected. This command does not group independent rooms together and does not
replace their queues.

Mute and volume changes address only the selected rooms. An absolute volume
asks for the same level in each selected room; a relative change is applied
from each room's own current level. The daemon applies policy per room.
Returned notes identify clamps and skipped rooms. A successful response may
be partial: it is not a promise that every room changed. Changing volume or
mute can end an active sleep fade, as with the ordinary room controls.

One confirmation makes **one existing daemon set-command request**, not a loop
of browser writes. The daemon retains its existing policy, action-log, and
undo behavior. This screen never changes the caller identity or grants rights.

## Authoritative target resolution

`GET /rooms/resolve?target=<encoded target>` uses the core resolver, with the
caller's default room and the current alias file. It returns:

- `target` and `matched`: the trimmed query and how it matched;
- `rooms`: selected, currently addressable room identities;
- `groups`: complete groups containing those rooms, including each member's
  household, primary player, and coordinator identity.

The route requires `resolve_rooms`, `list_rooms`, and `list_zones` read
permission. It reads topology but does not query playback or send speaker
commands. A cold surface may need an inventory survey. No new MCP tool is
introduced by this increment; existing MCP commands already accept the core's
smart targets.

The browser deliberately does not reconstruct aliases from the `/rooms`
listing. An alias may contain an unqualified room name shared by two
households, and the core can select both. A room's informational alias labels
are not enough to reproduce that behavior. Missing alias members are not
invented: only currently addressable rooms appear in the resolution.

Before dispatch, the browser resolves the same target again and requires the
same selected identities and complete groups. Editing the target, changing
an alias, regrouping, replacing a player, changing the default behind `here`,
or changing a resumed source requires review again. A prefix match is labeled
as such. These checks are **not a server-side reservation**: another controller
can still change the house after the final check and before execution.

## Pending actions and recovery

The panel acquires the affected groups' existing action lanes and reserves
native slider lanes before preflight. A pending volume, mute, or unsent slider
draft prevents the collective action from starting. Replacement cards inherit
volume/mute exclusion until the original operation settles. No pending slider
draft is silently executed as part of a collective command.

Closing the panel or hiding the page consumes its review. A command already
sent retains its pending slot and eventual receipt; it is not canceled on the
speaker merely because its view closed. Lost or malformed replies remain
uncertain and are never automatically retried. Check the current house before
sending another action. No failed set command is split into fallback requests.

## Verification boundary

`scripts/test-remote-room-sets.cjs` exercises the shipped controller, validation,
request lifecycle, slider/group coordination, studio registration, and an actual
localhost HTTP exchange using a synthetic responder. Rust tests in
`surface/players/targets/tests.rs` exercise the resolver contract and admission
before I/O, with the HTTP query parser tested beside its route.

Local component checks exercise the shipped browser initializers and navigation
with synthetic API replies. Together with the Rust and Node regressions above
they establish software behavior, not physical-Sonos operation; hardware
acceptance remains separate.
