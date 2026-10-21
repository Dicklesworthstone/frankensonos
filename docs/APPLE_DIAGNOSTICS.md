# Native setup diagnostics

Open **House → Setup diagnostics**, or **House options → Setup diagnostics**,
then choose **Run setup checks**. No selected room, discovered speaker, healthy
SSE connection or playing DJ is required. The configured daemon must still be
reachable: a connection failure is not a diagnostic verdict about its LAN.

The native client makes one `GET /doctor` through its existing bounded transport.
The daemon decides which checks are registered and permitted for this caller.
The remote does not fabricate a complete local-machine or tailnet test suite.
It sends no playback, repair, sign-in, library-sync or setup mutation. It neither
executes a suggested remedy nor turns evidence into a URL to fetch.

## Reading the report

The screen keeps the daemon's check IDs, execution order, summaries, details,
remedies, status and duration. **Needs attention** shows warnings and failures;
**Skipped** shows checks that were not verified. Local text filtering searches
IDs, titles, summaries and remedies without another request. Machine evidence
can be expanded per check.

The schema-1 report is checked for unique complete rows, supported status values,
matching counts and the correct diagnostic exit code (0, 6 or 7). HTTP 200 with
failed checks is a valid report, not an HTTP failure. An unsupported schema,
malformed response or contradictory total is an error, not a partial success.
An empty report says **No checks reported**; an all-skipped report says **No checks
ran**. Passing reported checks does not establish untested playback behavior.

**Received** is the client's receipt time, not the daemon's generation time.
The API can reuse a recent report. Starting another run, cancelling or hiding the
screen makes any retained observation a labelled **Previous report**. A failed
refresh does not relabel old results as current.

## Lifecycle and privacy

Opening, foregrounding and filtering never start checks automatically. Each
press of **Run setup checks** permits one read. Requests are single-flight;
cancelled work must drain before another run can begin. Cancellation before the
task starts prevents even the GET. Late responses after cancellation, hiding or
disconnection cannot replace the displayed result. No error is retried, and read
errors are not presented as uncertain playback writes.

The diagnostic request has a 180-second URLSession request timeout, longer than
normal display reads. **Stop waiting** cancels the client read; it does not claim
the daemon has stopped an already accepted check. Disconnection clears the
retained report. This client does not persist reports to disk or upload them.

**Share diagnostic text** exports the whole received report, regardless of the
filter. Machine evidence is excluded unless explicitly selected. Even summaries
and remedies can contain private room names, local paths or network addresses:
review before sharing. The export carries its receipt time and whether it is a
previous observation. It does not apply repairs or change the house.

## Validation boundaries

Run the repository's full native gates with:

```sh
swift test --package-path apps/apple -Xswiftc -warnings-as-errors
```

Twenty-three diagnostic contract, client and lifecycle tests passed three times
under Swift 6.2.1/Linux with warnings as errors. The focused harness used the
exact new production diagnostic files, the relevant production URLSession/client
implementation, parser, and model declarations. Unrelated playback/account
methods were excluded; this is not a full-package integration result.

A real loopback HTTP fixture exercised the production URLSession reader through
a failed-check report, a policy refusal, contradictory totals, an HTTP redirect
that was not followed, and an empty report. Exactly five bodyless `GET /doctor`
requests were observed, with no topology read, playback request or automatic
retry. These were synthetic daemon responses, not a running Rust diagnostic
engine. Both iOS17/macOS14 branches of the updated application source pass syntax
parsing. Apple SDK type-checking, rendered UI and share-sheet behavior, the full
Swift package and physical Sonos validation remain unexecuted locally.
