# Conditional scene edits

Saved-scene reads return an opaque `revision` for the complete stored
definition, including metadata omitted from the public display. A client can
carry that revision into a save or deletion so another controller's newer
definition is preserved.

## CLI

Read the scene with `fsonos scene show Evening`; the text output includes its
revision, and `--json` returns it as the `revision` field. Then use that
exact value:

```bash
fsonos scene save Evening --from evening.toml --expected-revision <revision>
fsonos scene save Evening --expected-revision <revision>
fsonos scene save Evening --backup-queues --expected-revision <revision>
fsonos scene rm Evening --expected-revision <revision>
```

The three save forms author a replacement, capture the current house, and
capture complete queue backups respectively. The revision protects the saved
definition; it does not freeze the speakers while a capture is prepared.

Use `--expected-revision absent` to create only when that name is unused.
Names retain the daemon's case-equivalence rules. Conditional writes work both
through the selected daemon and with `--direct`. A failed remote request
never writes a fallback copy in the CLI's local store.

## Native Apple remote

The iPhone, iPad and Mac remote retains the saved scene's revision through its
review and final confirmation. Capturing a replacement, saving an authored
replacement and deleting a scene all send that original revision, including
when only private stored metadata changed. Creating a scene sends `absent`,
so another controller's newly saved, case-equivalent name cannot be replaced.

A conflict displays **Scene changed — review again** and retains the original
draft. Refresh the saved scene and review the change again; the client never
retries with a revision the user has not reviewed. Lost replies and malformed
save acknowledgements remain uncertain. Successful saves must return a valid
new revision before the app reports completion.

Older daemon responses without revisions remain readable and can still be
used for playback. Replacing or deleting those scenes requires a daemon that
supplies a valid revision.

## MCP and HTTP

MCP `get_scene` and `list_scenes` expose each scene's revision in structured
content. Supply it as `expected_revision` to `save_scene` or `delete_scene`.
The option works with capture, `backup_queues`, and authored `scene`
definitions; it belongs beside `name`, not inside the authored definition.
Use the literal `absent` for a create-only save.

For HTTP, `PUT /scenes/{name}` and `DELETE /scenes/{name}` take
`expected_revision` in their JSON request body. A forwarded stdio MCP call
retains that body all the way to the daemon, including for DELETE.

A mismatch returns `CONFIG_CHANGED` without overwriting or deleting the
newer definition. Read the current scene and review a new change. Do not
automatically retry with its newer revision, because that would accept an edit
the caller has not reviewed. An uncertain reply still requires observation
before another mutation.

Omitting the option preserves existing caller behavior. Even those calls
compare the row observed at admission before the final store mutation, but an
explicit revision also protects the interval between the caller's review and
the start of the request. Applying a scene retains its existing separate
playback contract.

## Regression coverage

The CLI/MCP regression cases exercise create-only authoring, matching
replacement, stale captures and queue backups, stale deletion, successful
conditional removal, and preservation of the selected daemon's authority.
The real daemon/simulator cases use the committed e2e harness and SQLite store.
A localhost transport regression checks that stdio DELETE forwarding retains
the exact revision body, while tool tests check advertised schemas and
no-fallback behavior after refused, lost or malformed replies.

Targeted commands:

```bash
cargo test -p fsonos-cli --test e2e_mcp_daemon_config
cargo test -p fsonos-cli --bin fsonos scene_cmd::tests
cargo test -p fsonos-cli --bin fsonos stdio_deletion_retains
cargo test -p fsonos-mcp --lib tools::scenes
```

Native regressions in `NativeSceneManagementTests.swift` cover conflicts after
the final confirmation read, create-only name collisions, hidden metadata
changes, exact revision bytes, retained drafts and uncertain acknowledgements.
Run them with `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
