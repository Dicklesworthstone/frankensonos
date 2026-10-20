# Editing and navigating a long queue

Open a group's **Queue** in the web remote. The list shows 25 entries at a
time; editing is bound to the daemon's opaque queue revision and exact renderer
URIs, not to track titles. The group remains household-qualified on every read
and command.

## Select a complete range

Enter the first and last queue positions under **Select across queue pages**,
then choose **Read and select range**. Both ends are included. For example,
24 through 80 selects 57 entries, including those outside the displayed page.
The remote reads that range once and accepts it only when the returned length,
positions, revision and overlapping identities agree with the displayed queue.
An incomplete response does not select a shorter prefix.

You can also check individual entries and navigate to adjacent pages. Their
selection remains together only while the queue revision agrees. Use
**Show selection start** to return to its first selected entry, or
**Clear selection** to start over. The panel reports the total selected count
and how many entries are outside the current page.

A single edit can contain at most **100 consecutive entries**, matching the
existing daemon limit. A gap in the selection prevents movement and removal.
The remote does not divide a larger selection into multiple commands or infer
where an album or classical work begins and ends from its titles. Choose the
boundaries of the intended recording yourself.

## Review, then move or remove

**Review removal** lists every selected title, including off-page entries.
**Review move**, **To start…**, and **To end…** use the existing move review.
A numeric move destination means *before that position in the original queue*,
not the position after the selected block has been removed. The review states
where the block will end up. Duplicate occurrences of the same URI remain
separate entries in their original order.

Confirmation sends one native block-edit request with the exact selected URIs
and queue revision, through the ordinary playback command coordination. It
never clears and rebuilds the queue. Movement does not start playback; removal
of the current entry may advance or stop it. Neither operation is undoable,
and an active DJ may subsequently add more music.

Navigation, changing the range, closing the queue or a stale live observation
invalidates an earlier confirmation. Navigating does not silently confirm it;
review again before sending. **Refresh queue** deliberately clears selections,
even when the newly read revision matches. A failed or malformed write reply
remains uncertain and is not repeated; use an explicit fresh read before
considering another edit.

## Find your place without changing playback

**Go to position** reads the 25-entry page containing the specified position.
Typing a position makes no request. **Show current entry** first reads the
current queue pointer, then reads one page from that same revision. These are
read-only navigation actions, not Play or Seek. If playback advances outside
that page during the read, the panel says so rather than continuously chasing
it. When no current entry is reported, it does not guess that the group should
start its queue.

Each queue card admits only one range or navigation operation at a time.
Closing or backgrounding it invalidates the result but retains its request
slot until the underlying read settles. A removed card cannot adopt late
output. Live updates use the existing shared refresh path; these controls add no periodic polling.

## Verification boundary

The navigation suite runs with
`node --test scripts/test-remote-queue-navigation.cjs` and is included in the
existing web queue CI step. Its localhost HTTP fixture verifies browser
request ordering and payloads, not physical-speaker behavior. Actual Sonos,
full daemon integration, and Rust workspace checks remain separate gates.
