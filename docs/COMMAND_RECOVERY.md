# Command recovery does not restart completed stages

A source start or playlist replacement contains several speaker requests. A
refusal or lost reply from a later request does not roll back the earlier ones.
The outer command-recovery path now observes those requests as they execute,
including the lower-level source helper's read-back after a lost reply.

An initial UPnP 800 can still trigger the existing topology refresh and retry
on a confirmed new coordinator. A single repeat-safe state write can still
recover at a player's newly discovered address. Coordinator and address
recovery share one retry budget: a failed coordinator retry cannot dispatch a
third command to the original player.

Once an earlier write may have taken effect, the outer path preserves the
original failure and reports that earlier steps may have applied. It does not
resend the source selection, queue edits or playback command from the start.
Known reads do not count as writes. Typed and raw SOAP 800 refusals are both
recognized; other write replies, including lost replies and Leave/402, are
conservatively treated as possible effects.

This is not transaction rollback, household isolation, or an assertion that
all lower-level orchestration is atomic. Inspect playback and the action log
before issuing another command after a partial failure. The original error
code and UPnP cause remain available. An error's transient `retryable` category
is not permission to replay a partially completed mutation.

Regressions: `cargo test --locked -p fsonos-api --lib heal::` and
`cargo test --locked -p fsonos-api --test command_recovery_sim --test heal_sim`.
Unit tests cover progress classification, raw and typed faults, uncertain
writes and transport forwarding. The command-recovery simulator tests exercise
late coordinator changes, source success followed by a lost Play reply,
one shared recovery budget, positive coordinator/address recovery, and
read-back completion after an accepted write loses its acknowledgement.
Physical-speaker acceptance is separate. Rust tests were not executed in the
editing environment, which has no Rust toolchain.
