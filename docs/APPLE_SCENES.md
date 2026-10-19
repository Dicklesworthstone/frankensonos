# Scenes in the native Mac remote

The Scenes panel reads the daemon's saved house scenes and applies them through
`CommandReviewSession`. It does not implement another scene engine or reinterpret
room references. Definitions keep their stored household-qualified names, group
coordinators and members, source URIs or favorites, DJ moods, play/pause settings,
levels and mutes.

Expand a scene to inspect its complete definition. **Review scene** reads the
house and the saved definition; **Confirm once** reads them again before sending
one `POST /scenes/{encoded-name}/apply` with an empty JSON object. The selected
room anchors the review, but never restricts the scene's house-wide scope. Both
the confirmation and the persistent result make that distinction explicit.

Definitions changed since the list was read must be loaded and reviewed again.
Failed list reads remove stale scene rows rather than leaving them actionable.
Leaving the panel fences delayed responses. A scene already sent retains its
receipt under its own name after a selection change. Lost or malformed replies
remain uncertain; refresh never repeats the apply. Successful replies may omit
empty arrays; partial replies retain every failed step, skipped room and policy
note. The review scrolls independently of its confirmation buttons.

This uses the existing conservative scene preflight: all currently observed
house groups must be readable and unchanged between review and confirmation.
It is not an atomic reservation against another controller. The daemon remains
authoritative for resolving stored room references, applying policy, and reporting
partial device outcomes. Creating/editing/deleting scenes remains on CLI/web.

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Twelve tests execute the production codecs, read/command actors and screen model
with synthetic daemon replies. They cover cross-household apply, exact path and
body encoding, changed definitions, partial and uncertain receipts, delayed reads,
suspension and selection changes. Linux execution does not validate SwiftUI;
the existing Native Mac remote workflow builds the UI on Apple SDKs. Physical
speakers and an actual tailnet remain separate acceptance checks.
