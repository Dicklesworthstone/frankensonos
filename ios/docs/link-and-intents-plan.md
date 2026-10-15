# Play a Spotify link, and Siri / Shortcuts

Two ways to start music without the library browse. Neither needs a Spotify Client ID: the daemon sends the speaker a
`spotify:` URI and the speaker plays it through the account linked in the Sonos app.

```
Spotify app ─copy link─▶ [Paste a Spotify link] ─▶ SpotifyLink.parse ─▶ POST /play {source_uri}
Siri / Shortcuts ─▶ App Intent ─▶ RoomRemote ─▶ POST /pause | /resume | /play/favorite {zone}
```

The daemon plays tracks today. Albums need the daemon change in `album-play-daemon-report.md`. Until it ships, the
daemon's own refusal is shown to the person.

## Pasting a link (`SpotifyLink`)

| # | Input | What the app does | Told |
|---|---|---|---|
| C1 | `spotify:track:<id>` | sends it as is | "Playing in <room>" |
| C2 | `https://open.spotify.com/track/<id>?si=…` | strips the query and fragment, sends `spotify:track:<id>` | same |
| C3 | `https://open.spotify.com/intl-de/album/<id>` | drops the locale segment | same |
| C4 | kind other than track, album, playlist, artist, episode, show | nothing sent | "That isn't a Spotify track or album link." |
| C5 | `https://spotify.link/…` | nothing sent | "Open the short link in Spotify first, then share the open.spotify.com one." |
| C6 | id that is not 22 base62 characters | nothing sent | "That Spotify link looks cut off." |
| C7 | text that is not a Spotify link | nothing sent | "That isn't a Spotify link." |
| C8 | whitespace around the link, or text before it | the first Spotify link in the text is used | |
| C9 | no room selected, or the room is offline | nothing sent | "Pick a room first." |
| C10 | the daemon refuses (policy, not implemented, no Spotify favorite) | the room is put back | the daemon's text and hint |

`PasteButton` reads the clipboard without iOS's paste prompt, and disables itself when the clipboard holds no text.

## Intents (`RoomRemote`)

Pause, resume and play a Sonos favorite by name, in a room by name. The intents run in the app's process and read the
daemon URL the app saved.

| # | State or input | What happens | Told |
|---|---|---|---|
| I1 | room not in the daemon's room list | nothing sent | "I don't know a room called X. Rooms: …" |
| I2 | room name in other case, or with spaces around it | matches | |
| I3 | favorite unknown | nothing sent | "No favorite called X in Y." |
| I4 | favorite name matches several | nothing sent | the matches, by name |
| I5 | favorite name is a unique prefix or substring | plays it | |
| I6 | daemon unreachable | nothing sent | "I can't reach the daemon." |
| I7 | no daemon chosen yet | nothing sent | "Open FrankenSonos and choose a daemon first." |
| I8 | daemon refuses (policy) | nothing changes | the daemon's text |
| I9 | pause / resume | exactly `{zone: <room>}` to `/pause` or `/resume` | "Paused Kitchen." |
| I10 | play favorite | `{zone: <room>, favorite: <id>}` to `/play/favorite` | "Playing Jazz Mix in Kitchen." |

The pure parts (`SpotifyLink.parse`, room and favorite matching, request bodies) are E2E rows against the stub daemon.
Only the Siri phrasing and the system paste control need a device.
