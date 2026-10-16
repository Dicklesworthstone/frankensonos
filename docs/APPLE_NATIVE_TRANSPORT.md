# Apple native transport bounds

The Swift client treats every daemon reply as untrusted network input.

Ordinary JSON responses are limited to 4 MiB. On Apple platforms the
URLSession transport reads response bytes incrementally, rejects an announced
Content-Length above the limit before buffering, and aborts once a chunked or
misreported response crosses the same hard cap. Cancellation is checked while
bytes arrive.

The Linux FoundationNetworking fallback used by package tests does not expose
URLSession.AsyncBytes, so it applies the same metadata and final-size contract
after URLSession returns the body. Shipping iOS and macOS clients use the
streaming branch.

Server-sent events remain incremental and use the existing EventParser bounds;
this change does not buffer the event stream or alter its 128-event consumer
buffer.

No command retry was added. A write that fails while receiving its
acknowledgement still becomes ClientError.uncertain and must be reconciled from
fresh state before another command.

The macOS binary workflow now runs:

    swift test --package-path apps/apple

before the Rust build, so the native package's request, response, model, SSE,
and command-contract tests are exercised on an Apple SDK.
