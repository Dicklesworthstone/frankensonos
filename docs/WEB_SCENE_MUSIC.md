# Choose music while building a scene

In the full web remote, open **Routines → Scenes**, expand **Manage scenes**,
and choose **New custom scene**. Add a group, choose its coordinator and member
rooms, then choose its music source. The coordinator is household-qualified;
two rooms with the same name in different households have independent choices.

## Sonos favorites

Choosing **Sonos favorite** reads the selected coordinator's household favorites.
Browse the cards or use **Find a favorite** to filter titles and descriptions.
The list shows six choices per page; **Previous choices** and **Next choices**
move through the matching results. Searching and paging do not contact the
daemon or change speakers.

Each card shows the favorite's title, description when available, and whether
it is a track, radio/stream, or album/playlist. Select a card with a click, tap,
Enter or Space. The **Selected favorite** summary remains visible when the
selected item is on another page or does not match the current search.

Only playable favorites with unique titles are offered. Scenes resolve saved
favorites by title, so two favorites with the same case-insensitive title cannot
be disambiguated by their IDs at application time. The chooser reports those
duplicates; rename them in Sonos before using them in a scene. Unsupported
favorite shortcuts are omitted. An empty list explains how to add a favorite
and refresh. Failed reads offer **Retry favorites** without discarding the scene
draft or a prior selection.

## DJ moods and music links

Choosing **DJ** offers **Automatic DJ** and the named moods returned for the
selected coordinator. Named moods can also be searched and paged. Automatic DJ
follows current steering and the house's time-of-day program; it remains
available when there are no named moods or the mood list cannot be read.

**Music URI or Spotify link** retains its direct-entry field. Spotify track,
album and playlist links are normalized by the existing scene authoring path.
The chooser does not add an external music search or contact Spotify.

## Saving and changing rooms

Selecting music only edits the custom scene draft. **Review custom scene** shows
the exact groups, sources, playback settings and optional room levels. Only
**Save custom scene** sends the existing `PUT /scenes/{name}`. Applying the saved
scene is a separate reviewed action and can regroup rooms or change playback.

Favorite choices are bound to their catalog item and destination. A favorite
that changes between selection and review must be refreshed and chosen again.
The existing review and confirmation also reread dependencies before saving.
Changing a coordinator clears that group's source and membership choices;
responses from the previous coordinator, a removed group, or a closed/discarded
draft cannot populate its replacement. Editing unrelated room settings does
not discard an in-progress source read.

This chooser authors new definitions. It does not import captured scenes for
editing or reconstruct their private renderer metadata.

## Verification

```sh
node --test scripts/test-remote-scenes.cjs scripts/test-remote-scenes-browser.cjs
FSONOS_SCENES_CHROMIUM=/path/to/chromium node --test scripts/test-remote-scenes-browser.cjs
```

The regular suite exercises source selection, search/paging, household
separation, stale read ownership, retry, favorite changes, automatic/named DJ,
and the existing capture/apply/delete/custom-scene behavior. A real localhost
JSON fixture verifies browsing and reviewed saving across the HTTP boundary.

The browser suite requires Playwright and an explicit Chromium executable;
ordinary Node runs skip it when `FSONOS_SCENES_CHROMIUM` is unset. It uses the
shipped scene component, DOM helper and styles with synthetic replies. Its
matrix covers 320, 390, 768 and 1440 pixels in light and dark themes, keyboard
selection, touch target sizes, filtering, paging, retry, selected summaries,
review/save, empty catalogs, and the unchanged Spotify URI path. No physical
speakers or Spotify account are used.
