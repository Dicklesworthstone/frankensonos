# Error codes

Every FrankenSonos surface reports a failure the same way: a stable `code`, a
specific `detail`, a one-sentence `hint` on what to do next, and `suggestions`
to retry with (the nearest room names, say). Codes are never renamed; new ones
are added at the end.

- **HTTP API**: the status below, with a JSON body:

  ```json
  {
    "detail": "unknown room \"Kichen\"; known rooms: Kitchen@S1, Den@S1, Patio@S2",
    "code": "UNKNOWN_ROOM",
    "hint": "Use a suggested room, or list rooms with list_zones (GET /zones).",
    "suggestions": ["Kitchen@S1"],
    "retryable": false
  }
  ```

  `UPNP_FAULT` adds `"upnp_code"` (the speaker's UPnP error number). Errors
  the HTTP framework raises itself, such as an unknown route, carry only
  `detail`.
- **MCP**: a tool error whose text reads
  `CODE: detail. Hint: ... Did you mean: a, b?`, with the JSON body above as
  structured content.
- **CLI**: `error[CODE]: detail`, then the hint and suggestions on stderr, and
  the exit code below. Command-line usage errors also exit 2.

`retryable` means the same request, sent again unchanged a little later, can
succeed.

| Code | HTTP | Exit | Retryable | Meaning | Hint |
|---|---|---|---|---|---|
| `INVALID_ARGUMENT` | 422 | 2 | no | A request field is missing, malformed or out of range. | Fix the field the detail names and send the request again. |
| `UNKNOWN_ROOM` | 404 | 3 | no | No room matches the name given. | Use a suggested room, or list rooms with list_zones (GET /zones). |
| `AMBIGUOUS_ROOM` | 409 | 2 | no | The name matches rooms in more than one place. | Repeat the request with one of the suggestions; Room@S1 or Room@S2 picks the household. |
| `UNKNOWN_HOUSEHOLD` | 404 | 3 | no | No household matches the label or id given. | List rooms with list_zones (GET /zones) to see the households. |
| `CROSS_HOUSEHOLD_GROUP` | 422 | 2 | no | The rooms are in different households, which can never share a group. | Group rooms within one household; S1 and S2 players can never share a group. |
| `NOT_READY` | 503 | 4 | yes | Nothing has been discovered yet. | Discovery is still running; retry in a few seconds. |
| `PLAYER_UNREACHABLE` | 503 | 4 | yes | A speaker did not answer. | Check the speaker is powered and on the network, then retry. |
| `NOT_COORDINATOR` | 409 | 4 | yes | The group changed under the command; its coordinator moved. | The group changed while the command ran; retry it. |
| `UPNP_FAULT` | 502 | 1 | no | A speaker answered with a UPnP fault (`upnp_code`) or an unreadable response. | The speaker refused the command in its current state; check it and retry. |
| `SPOTIFY_NOT_LINKED` | 409 | 1 | no | The household's Sonos app has no linked Spotify account. | Link Spotify in that household's Sonos app once, then retry. |
| `RENDER_PARAMS_MISSING` | 409 | 1 | no | The household's Spotify render parameters have not been learned. | Add any Spotify track to My Sonos in that household's app, then retry. |
| `SPOTIFY_AUTH_REQUIRED` | 409 | 1 | no | The daemon has no valid Spotify sign-in. | Sign in to Spotify on the daemon host, then retry. |
| `POLICY_DENIED` | 403 | 5 | no | The house policy forbids the request. | The house policy forbids this; ask the owner to change it. |
| `UNKNOWN_MOOD` | 404 | 3 | no | No DJ mood has that name. | Use one of the suggested moods. |
| `NO_DJ_SESSION` | 404 | 3 | no | No DJ session runs in that zone. | Start the DJ in that zone first (dj_start). |
| `INTERNAL` | 500 | 1 | no | A fault inside the daemon. Details stay in the daemon log. | Retry once; if it persists, check the daemon log. |
| `NOT_IMPLEMENTED` | 501 | 1 | no | The request is understood but this build cannot carry it out yet. | Use what the detail suggests until this lands. |

## Notes

A successful response can carry notes about how the request was carried out.

| Code | Meaning |
|---|---|
| `VOLUME_CLAMPED` | The requested volume exceeded the house policy and was lowered. |
