# Manage aliases and default rooms on the house controller

Room aliases name one or more physical rooms. Defaults decide which physical
room `here` means, globally or for a particular policy client. These settings
live in the selected daemon's `aliases.toml` and take effect on the next target
resolution. They do not group speakers or immediately change playback.

The phone remote, dashboard, CLI, HTTP API and MCP can read and change this
configuration without filesystem access to the daemon host. The complete read
includes saved targets that are offline, unlike the discovered-room list. Each
mutation changes one alias or default, retaining other settings and comments.

## Phone remote and dashboard

On the phone, open **House & sound**, then **Edit aliases and default rooms**.
On the dashboard, choose **Aliases & default rooms** beside the room controls.
Both surfaces use the same editor:

1. Read the current aliases, house default and explicit client defaults.
2. Create an alias, edit its members, remove it, or set/clear a default. Room
   suggestions come from discovery; exact offline names and existing client
   identities remain available. Repeated members and their order are retained.
3. Review the current setting and proposed change, then save it once.

An alias edit changes its members while preserving its name. Clearing a client
default restores the house fallback, when one exists. Changes affect future
commands and routines; saving does not start music or regroup speakers.

The editor retains the original revision throughout review. A changed
configuration requires a fresh edit; it never silently rebases the draft. If
the acknowledgement is lost or unreadable, the draft and outcome stay visible.
Use **Refresh room configuration** to observe the saved state before deciding
on another change. A playback refresh cannot clear this uncertainty, and the
browser never repeats the earlier mutation automatically.

## CLI

Existing commands now use the selected daemon. With `--daemon-url`, the change
reaches that house controller over its HTTP/Tailscale connection; with
`--daemon`, a running daemon is required. Use `--direct` to edit this machine's
local data directory without contacting speakers or reading discovery seeds.

```sh
fsonos --daemon --json rooms configuration
fsonos --daemon rooms alias add downstairs 'Kitchen@S1' 'Living Room@S2'
fsonos --daemon rooms default 'Office@S2'
fsonos --daemon rooms default 'Bedroom@S2' --client 'tag:bedside'
fsonos --daemon rooms default --client 'tag:bedside'
fsonos --daemon rooms default --client 'tag:bedside' --clear
fsonos --daemon rooms alias rm downstairs
```

For an edit based on a previously reviewed read, supply its exact revision:

```sh
fsonos --daemon rooms alias add downstairs 'Kitchen@S1' 'Office@S2' \
  --expected-revision '<revision from rooms configuration>'
```

Alias add/remove and default set/clear all accept `--expected-revision`. When
omitted, the CLI reads the selected configuration once immediately before
submitting the mutation. A conflict is returned to the caller; the command
does not reread and retry. Invalid names or changes are rejected before daemon
discovery. A selected daemon's failure never falls back to a local file edit.

`rooms configuration` returns all aliases/defaults, warnings and the current
revision. `rooms default` prints the effective default for the requested
client, including global fallback; with `--json`, it returns that same complete
configuration. Mutations print the API's `RoomConfiguredDto` with `--json`.

## HTTP and MCP

`GET /rooms/configuration` returns:

```json
{
  "revision": "<opaque observed revision>",
  "aliases": [
    {"name": "downstairs", "rooms": ["Kitchen@S1", "Living Room@S2"]}
  ],
  "default_room": "Office@S2",
  "client_defaults": {"tag:bedside": "Bedroom@S2"},
  "warnings": []
}
```

`POST /rooms/configuration` takes the observed revision and one change:

```json
{
  "expected_revision": "<opaque observed revision>",
  "change": {
    "kind": "set_alias",
    "name": "downstairs",
    "rooms": ["Kitchen@S1", "Office@S2"]
  }
}
```

The four changes are:

| `kind` | Additional fields | Result |
| --- | --- | --- |
| `set_alias` | `name`, nonempty `rooms` | Create an alias or replace its members |
| `remove_alias` | `name` | Remove that alias |
| `set_default` | `room`, optional `client` | Set the global default or one client's explicit override |
| `clear_default` | optional `client` | Clear the global default or one client's override |

Omit `client` for the global default. Clearing a client override restores its
global fallback; it does not clear any other client's setting. Client keys are
exact policy identities. Alias matching follows the existing case/apostrophe
normalization, and editing an existing alias retains its authored name.

A successful response contains `done`, `changed`, and the complete accepted
`configuration`. The MCP Full profile exposes **`room_configuration`** and
**`configure_rooms`** with the same read and mutation contracts. The write tool
takes `expected_revision` and the typed `change` object. Unknown fields and
unsupported changes are rejected.

## Permissions and target behavior

Reading uses the `room_configuration` permission. Changing the document needs
`configure_rooms`, in `house-config`, plus ownership administration: the local
CLI, the daemon's observed tailnet owner, or a client explicitly configured
with `manage_others = true`. A tool denial still takes precedence. A delegated
client's general permission to control rooms does not grant it permission to
rewrite everyone else's defaults.

Aliases and defaults name physical rooms or player IDs, not other aliases.
`all`, `everywhere` and `here` keep their reserved meanings. Offline targets
remain editable and are retained verbatim. Warnings report names that the
available inventory cannot currently resolve, ambiguous names, or names
shadowed by a physical room. Reads and edits use only already available
inventory; they never start discovery or send a speaker command. A warning at
line zero means no inventory was available to check the targets.

Changes affect subsequent commands and scheduled routines that use these
names. Removing an alias can leave a saved routine's target unresolved; there
is no automatic rewriting of saved scenes or schedules. Already prepared
commands may finish using the targets they resolved before the edit.

## Persistence, conflicts and recovery

The revision represents the exact saved bytes, including comments, and
distinguishes a missing file from an existing empty file. The shared writer
locks a stable sibling file, compares the revision, applies the one change,
validates the complete result, and publishes it atomically. Identical changes
retain the original bytes and revision. Unrelated aliases, defaults and
comments are preserved. Unusual inline/dotted layouts that cannot safely
support the requested insertion or removal are refused without rewriting the
file; value updates use the parser's exact spans where possible.

All built-in writers use this lock. A text editor that does not honor the lock
can still race after the final comparison; the filesystem does not offer an
atomic compare-and-rename against an uncooperative external writer.

A stale revision returns `CONFIG_CHANGED` (HTTP 409), without retrying. If the
file was published but its directory sync cannot be confirmed, the result is
`OUTCOME_UNCERTAIN`. Read the selected daemon's configuration before deciding
on another change. A read reports current state; it cannot prove exactly how
a previous lost request completed. Never replay an uncertain mutation blindly.

Changed configuration writes are explicit non-undoable history barriers.
Playback undo cannot reach through them to restore an older action. Failure to
write the separate action log does not erase a confirmed configuration result.

## Verification

Core tests cover exact-byte revisions, comments and multiline values,
no-ops, invalid/unsupported edits, persistence, and competing file writers.
API and real-socket HTTP tests cover permissions, resolution, defaults,
conflicts, strict schemas and operation without network I/O. MCP tests cover
all four changes and terminal remote errors. The CLI daemon scenario edits
from a separate data directory, confirms that the local file remains
untouched, controls the selected rooms through the new alias, and reopens the
saved configuration offline.

The browser regression suite exercises all four changes on both surfaces,
review and draft retention, exact client identities, no-op receipts, stale
revisions, unrelated-setting preservation, lost acknowledgements, stored
command coordination, and real loopback HTTP requests. It also covers panel
focus and plain-text rendering of saved names.

Executed test results are reported separately from tests added to the source.
