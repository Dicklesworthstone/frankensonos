import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let catalogID = "0123456789ABCDEFGHIJKL"
private func sourceQueueCommands() -> [NativeCommand] {
    [.playSource("spotify:artist:\(catalogID)"), .playSource("spotify:show:\(catalogID)"),
     .queueAdd("spotify:track:\(catalogID)", next: true), .queueAdd("spotify:album:\(catalogID)", next: false)]
}

@Suite("Source queue review integration") struct SourceQueueReviewTests {
    @Test func catalogReplacementsAndInsertionsReadOneEntryBeforeReviewAndConfirmation() async throws {
        for command in sourceQueueCommands() {
            for household in ["TEST-A", "TEST-B"] {
                let transport = ReviewTransport(), zone = reviewZone(house: household)
                let session = try CommandReviewSession(client: reviewClient(transport))
                let review = try await session.prepare(command, in: zone)
                #expect(await transport.writes().isEmpty)
                _ = try await session.execute(review)
                await #expect(throws: ContractError.self) { try await session.execute(review) }
                #expect(await transport.writes().count == 1)
                let reads = await transport.requests.filter { $0.httpMethod == "GET" && $0.url?.path.hasSuffix("/queue") == true }
                #expect(reads.count == 2)
                for read in reads {
                    #expect(read.url?.path == "/zones/Studio@\(household)/queue")
                    #expect(URLComponents(url: read.url!, resolvingAgainstBaseURL: false)?.queryItems == [
                        URLQueryItem(name: "offset", value: "0"), URLQueryItem(name: "limit", value: "1")
                    ])
                }
            }
        }
    }

    @Test func changedQueueConsumesReviewWithoutSending() async throws {
        for command in sourceQueueCommands() {
            let transport = ReviewTransport()
            let session = try CommandReviewSession(client: reviewClient(transport))
            let review = try await session.prepare(command, in: reviewZone())
            await transport.setRevision("queue-r2")
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func sameRevisionCannotHideChangedCountOrFirstOccurrence() async throws {
        for reply in [
            #"{"zone":"Studio","total":2,"revision":"queue-r1","playing":1,"items":[{"position":1,"title":"Entry","uri":"test:entry"}]}"#,
            #"{"zone":"Studio","total":1,"revision":"queue-r1","playing":1,"items":[{"position":1,"title":"Entry","uri":"test:replacement"}]}"#
        ] {
            let transport = ReviewTransport()
            let session = try CommandReviewSession(client: reviewClient(transport))
            let review = try await session.prepare(.queueAdd("spotify:track:\(catalogID)", next: false), in: reviewZone())
            await transport.overrideQueue(reply)
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func appendAllowsPlaybackAdvanceButNextAndReplacementRetainTheirOccurrence() async throws {
        for command in sourceQueueCommands() {
            let transport = ReviewTransport()
            await transport.setQueue(total: 3, playing: 1)
            let session = try CommandReviewSession(client: reviewClient(transport))
            let review = try await session.prepare(command, in: reviewZone())
            await transport.setQueue(total: 3, playing: 2)
            if command.sourceQueueNeedsOccurrence {
                await #expect(throws: ContractError.self) { try await session.execute(review) }
                #expect(await transport.writes().isEmpty)
            } else {
                await transport.setURI("test:natural-next-track")
                _ = try await session.execute(review)
                #expect(await transport.writes().count == 1)
            }
        }
    }

    @Test func emptyRetainedQueuesRemainUsable() async throws {
        for command in sourceQueueCommands() {
            let transport = ReviewTransport()
            await transport.setQueue(total: 0, playing: nil)
            let session = try CommandReviewSession(client: reviewClient(transport))
            _ = try await session.execute(session.prepare(command, in: reviewZone()))
            #expect(await transport.writes().count == 1)
        }
    }

    @Test func unreadableOrMismatchedDestinationCannotAuthorizeAWrite() async throws {
        let cases = [
            #"{"zone":"Other","total":0,"revision":"r1","items":[]}"#,
            #"{"zone":"Studio","total":0,"revision":null,"items":[]}"#,
            #"{"zone":"Studio","total":0,"revision":" ","items":[]}"#,
            #"{"zone":"Studio","total":1,"revision":"r1","items":[]}"#,
            #"{"zone":"Studio","total":0,"revision":"r1","playing":1,"items":[]}"#,
            #"{"zone":"Studio","total":1,"revision":"r1","playing":0,"items":[{"position":1,"title":"Entry"}]}"#,
            #"{"zone":"Studio","total":1,"revision":"r1","playing":2,"items":[{"position":1,"title":"Entry"}]}"#,
            #"{"zone":"Studio","total":1,"revision":"r1","items":[{"position":2,"title":"Entry"}]}"#,
            #"{"zone":"Studio","total":2,"revision":"r1","items":[{"position":1,"title":"A"},{"position":2,"title":"B"}]}"#
        ]
        for afterReview in [false, true] {
            for reply in cases {
                let transport = ReviewTransport()
                let session = try CommandReviewSession(client: reviewClient(transport))
                let command = NativeCommand.playSource("spotify:show:\(catalogID)")
                let review = afterReview ? try await session.prepare(command, in: reviewZone()) : nil
                await transport.overrideQueue(reply)
                if let review {
                    await #expect(throws: (any Error).self) { try await session.execute(review) }
                } else {
                    await #expect(throws: (any Error).self) { try await session.prepare(command, in: reviewZone()) }
                }
                #expect(await transport.writes().isEmpty)
            }
        }
    }

    @Test func cancelledFinalQueueReadCannotSendOrReleaseTheSharedLaneEarly() async throws {
        let transport = ReviewTransport()
        let session = try CommandReviewSession(client: reviewClient(transport))
        let review = try await session.prepare(.queueAdd("spotify:track:\(catalogID)", next: true), in: reviewZone())
        await transport.blockNext("/zones/Studio@TEST-A/queue")
        let pending = Task { try await session.execute(review) }
        try await waitForReviewGate(transport)
        await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: reviewZone()) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await pending.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func lostAcknowledgementRemainsUncertainAndIsNeverRetried() async throws {
        let transport = ReviewTransport()
        let session = try CommandReviewSession(client: reviewClient(transport))
        let review = try await session.prepare(.playSource("spotify:artist:\(catalogID)"), in: reviewZone())
        await transport.loseWrite()
        do { _ = try await session.execute(review); Issue.record("Expected an uncertain write") }
        catch let error as ClientError {
            guard case .uncertain = error else { Issue.record("Lost reply was not uncertain"); return }
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }

    @Test func ordinarySourcePlaybackDoesNotRequireAnUnrelatedQueue() async throws {
        for kind in ["track", "album", "playlist", "episode"] {
            let transport = ReviewTransport()
            await transport.overrideQueue("unreadable")
            let session = try CommandReviewSession(client: reviewClient(transport))
            _ = try await session.execute(session.prepare(.playSource("spotify:\(kind):\(catalogID)"), in: reviewZone()))
            #expect(await transport.requests.filter { $0.httpMethod == "GET" && $0.url?.path.hasSuffix("/queue") == true }.isEmpty)
            #expect(await transport.writes().count == 1)
        }
    }
}
