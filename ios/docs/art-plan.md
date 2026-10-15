# Real album art

The app draws a generated cover because the daemon never sends the real one. A speaker reports where its art lives
(`albumArtURI` in the track metadata); `fsonos-proto` already parses it (`didl.rs`, `album_art_uri`) and
`fsonos-core` keeps it (`playback.rs`, `favorites.rs`), but the HTTP API drops it from `TrackDto`.

```
speaker ──albumArtURI──▶ daemon ──art_url──▶ app (AsyncImage)
   /getaa?s=1&u=...        │   "/speaker-art?player=RINCON_…&u=%2Fgetaa%3F…"   (a relative URL; the app resolves it
   or https://cdn…         │    or the https URL itself                  against the daemon's address)
                           └── GET /art fetches the image from that speaker's :1400 and returns the bytes
```

## Contract

| Change | Behavior |
|---|---|
| `TrackDto.art_url` (`GET /zones/{room}/state`, events refetch) | `string` or absent. An `https` URL is passed through. A path on a speaker (`/getaa...`) or an `http` URL on a speaker's address becomes `/speaker-art?player=<player id>&u=<the path and query, percent-encoded>` |
| `FavoriteDto.art_uri` | the same normalization |
| `GET /speaker-art?player=&u=` (operation id `get_art`) | fetches `http://<that player's address>:1400<u>` and returns the body with the upstream `content-type` (images only), `cache-control: public, max-age=86400`. Anything else is refused |

## Failure matrix

| # | State or input | What the daemon does | What the caller is told |
|---|---|---|---|
| A1 | `player` not a known player | no request | 404 `UNKNOWN_PLAYER` |
| A2 | `u` does not start with `/getaa`, has `//`, `..`, a scheme or an authority, or is over 1024 bytes | no request (no server-side request forgery: only the player's own `/getaa` is ever fetched) | 400 `INVALID_ARGUMENT` |
| A3 | Speaker unreachable or slow (5 s limit) | no retry loop | 502, `retryable: true` |
| A4 | Upstream is not `image/*` or is over 4 MiB | body dropped | 502 `BAD_ART` |
| A5 | Speaker answers 404 | nothing cached | 404 |
| A6 | A track with no art | `art_url` absent | the app keeps its generated cover |
| A7 | Policy | `get_art` needs the allow list like any read | `POLICY_DENIED` names it |
| A8 | The art route as a way to read other speaker pages | cannot: path prefix and the player's own address are fixed | covered by A2 tests with `..`, encoded `..`, `//host`, `@` |

## App

`TrackDTO.art_url` decodes to an optional string; `Track.live` takes an optional `URL` resolved against the daemon's
base URL; `AlbumArtworkView` shows `AsyncImage` and falls back to the generated cover while loading and on failure.
The Now Playing sheet, the mini player, room rows, the room switcher chips and Favorites all use it.
