import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func managedZone(_ household: String = "S1", coordinator: String = "Room + &", members: [String] = ["Room + &", "Hall"]) -> Zone {
    Zone(household: household, coordinatorRoom: coordinator, members: members, transportState: "playing")
}
private func managedPlaylist(_ title: String = "Evening", id: String = "SQ:7") -> SonosPlaylist {
    SonosPlaylist(id: id, title: title, artUri: nil)
}
private func managedJSON<T: Encodable>(_ value: T) throws -> HTTPReply {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    return HTTPReply(data: try encoder.encode(value), status: 200)
}
private func managedFields(_ request: URLRequest) throws -> [String: JSONValue] {
    try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody ?? Data())
}
private func managedString(_ value: JSONValue?) -> String? {
    if case .string(let string) = value { return string }; return nil
}

/// The fixture mutates only native saved catalogs. Real command/session/model
/// code owns all reviews, dependency reads, dispatch, uncertainty and lifetime.
private actor PlaylistManagementTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var topology = [managedZone(), managedZone("S2")]
    var catalogs = ["S1": [managedPlaylist()], "S2": [managedPlaylist("Other household")]]
    var queueRevision = "retained-1"
    var queueTotal: UInt32 = 3
    var savedRevision = "saved-1"
    var savedTotal: UInt32 = 3
    var sourceURI = "https://radio.example.test/live"
    var position: UInt32 = 1
    var logged = true
    var lost = false
    var cancelled = false
    var writeOverride: HTTPReply?
    var failRead: String?
    var blocked = false
    private var holdKey: String?
    private var waiter: CheckedContinuation<Void, Never>?
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func changeQueue(revision: String? = nil, total: UInt32? = nil, position: UInt32? = nil) {
        if let revision { queueRevision = revision }; if let total { queueTotal = total }; if let position { self.position = position }
    }
    func changeSaved(revision: String? = nil, total: UInt32? = nil) {
        if let revision { savedRevision = revision }; if let total { savedTotal = total }
    }
    func replace(_ rows: [SonosPlaylist], household: String = "S1") { catalogs[household] = rows }
    func regroup(_ value: [Zone]) { topology = value }
    func setLogged(_ value: Bool) { logged = value }
    func loseReply() { lost = true }
    func cancelReply() { cancelled = true }
    func overrideWrite(_ value: HTTPReply) { writeOverride = value }
    func fail(_ path: String?) { failRead = path }
    func hold(_ method: String, _ path: String) { holdKey = method + " " + path }
    func release() { blocked = false; let held = waiter; waiter = nil; held?.resume() }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let method = request.httpMethod ?? "GET"
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let response: HTTPReply
        if method == "GET" {
            if failRead == path { throw URLError(.cannotConnectToHost) }
            let address = query.first(where: { $0.name == "zone" })?.value ?? path
            let household = address.contains("@S2") ? "S2" : "S1"
            let zone = topology.first(where: { $0.household == household }) ?? managedZone(household)
            switch path {
            case "/zones": response = try managedJSON(topology)
            case "/sonos/playlists": response = try managedJSON(catalogs[household] ?? [])
            case "/sonos/playlists/contents":
                let id = query.first(where: { $0.name == "playlist" })?.value
                guard let item = catalogs[household]?.first(where: { $0.id == id }) else {
                    return HTTPReply(data: Data(#"{"code":"NO_MATCH","detail":"No saved playlist"}"#.utf8), status: 404)
                }
                let rows: [JSONValue] = (0..<savedTotal).map { index in
                    .object(["position": .integer(Int64(index + 1)), "title": .string("Saved track \(index + 1)"),
                             "uri": .string("fixture:track-\(index + 1)")])
                }
                response = try managedJSON(["zone": JSONValue.string(zone.coordinatorRoom),
                    "playlist": .object(["id": .string(item.id), "title": .string(item.title)]),
                    "revision": .string(savedRevision), "total": .integer(Int64(savedTotal)),
                    "offset": .integer(0), "next_offset": .null, "items": .array(rows)])
            default:
                if path.hasSuffix("/state") {
                    response = try managedJSON(ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false,
                        groupVolume: 20, track: .init(uri: sourceURI, title: "Live radio", creator: nil, album: nil,
                            durationSecs: nil, positionSecs: nil, queuePosition: nil), shuffle: false, repeat: "off"))
                } else if path.hasSuffix("/queue") {
                    let items: [JSONValue] = queueTotal == 0 ? [] : [.object([
                        "position": .integer(1), "title": .string("Retained first track"), "uri": .string("fixture:retained")])]
                    response = try managedJSON(["zone": JSONValue.string(zone.coordinatorRoom), "total": .integer(Int64(queueTotal)),
                        "revision": .string(queueRevision), "playing": queueTotal == 0 ? .null : .integer(Int64(position)),
                        "items": .array(items)])
                } else { throw ContractError.invalid("Unexpected playlist management read: \(path)") }
            }
        } else {
            let fields = try managedFields(request)
            let address = managedString(fields["zone"]) ?? ""
            let household = address.hasSuffix("@S2") ? "S2" : "S1"
            let coordinator = topology.first(where: { $0.household == household })?.coordinatorRoom ?? "Room + &"
            let original = catalogs[household] ?? []
            let item: SonosPlaylist
            var changed = true
            if path == "/sonos/playlists/save" {
                item = managedPlaylist(managedString(fields["title"]) ?? "", id: "SQ:8")
                catalogs[household, default: []].append(item)
            } else if path == "/sonos/playlists/rename" || path == "/sonos/playlists/remove" {
                let id = managedString(fields["playlist"])
                guard let index = original.firstIndex(where: { $0.id == id }) else { throw ClientError.unreadable }
                if path.hasSuffix("/rename") {
                    item = managedPlaylist(managedString(fields["title"]) ?? "", id: original[index].id)
                    changed = !item.title.utf8.elementsEqual(original[index].title.utf8)
                    catalogs[household]![index] = item
                } else { item = original[index]; catalogs[household]!.remove(at: index) }
            } else if path == "/pause" {
                return try managedJSON(["done": JSONValue.string("Paused"), "changed": .bool(true)])
            } else { throw ContractError.invalid("Unexpected playlist management write: \(path)") }
            var body: [String: JSONValue] = ["done": .string("Native catalog confirmed"), "zone": .string(coordinator),
                "playlist": .object(["id": .string(item.id), "title": .string(item.title)]), "logged": .bool(logged)]
            if !path.hasSuffix("/save") {
                body["operation"] = .string(path.hasSuffix("/rename") ? "rename" : "remove")
                body["changed"] = .bool(changed)
            }
            response = try writeOverride ?? managedJSON(body)
        }
        if holdKey == method + " " + path {
            holdKey = nil; blocked = true
            await withCheckedContinuation { waiter = $0 }
        }
        if method == "POST" && lost { lost = false; throw URLError(.networkConnectionLost) }
        if method == "POST" && cancelled { cancelled = false; throw CancellationError() }
        return response
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.yield(.connected) }
    }
}
private func managedClient(_ transport: PlaylistManagementTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func managedWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Playlist management test did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func managedWorkflow(_ transport: PlaylistManagementTransport) throws -> SonosPlaylistManagementWorkflow {
    let client = try managedClient(transport)
    let workflow = SonosPlaylistManagementWorkflow(client: client, commands: CommandReviewSession(client: client))
    workflow.resume(); return workflow
}

@Suite("Native saved-playlist management") struct NativePlaylistManagementTests {
    @Test func titleValidationMatchesDaemonBytesAndUnicodeWhitespace() throws {
        #expect(try SonosPlaylistMutation.validatedTitle("\u{85}  Dvořák, Bach & <friends>  \u{a0}") == "Dvořák, Bach & <friends>")
        #expect(try SonosPlaylistMutation.validatedTitle(String(repeating: "é", count: 511)).utf8.count == 1_022)
        #expect(try SonosPlaylistMutation.validatedTitle("\u{feff}Title") == "\u{feff}Title")
        for title in ["", " \n\t ", "A\0B", "A\nB", String(repeating: "é", count: 512)] {
            #expect(throws: ContractError.self) { try SonosPlaylistMutation.validatedTitle(title) }
        }
    }

    @Test func saveReviewsRetainedQueueWhileRadioPlaysAndCreatesDistinctSameTitle() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.save(title: " Evening ", in: managedZone()))
        #expect(review.trackCount == 3 && review.currentSource == "Live radio" && review.duplicateTitles == 1)
        #expect(await transport.writes().isEmpty)
        // Selection progress is not a contents change and must not block saving.
        await transport.changeQueue(position: 2)
        let receipt = try await session.execute(review)
        #expect(receipt.playlist.id == "SQ:8" && receipt.playlist.title == "Evening")
        #expect(await transport.catalogs["S1"]?.map(\.id) == ["SQ:7", "SQ:8"])
        let write = try #require(await transport.writes().first)
        #expect(write.url?.path == "/sonos/playlists/save")
        #expect(try managedFields(write) == ["zone": .string("Room + &@S1"), "title": .string("Evening")])
        #expect(await transport.writes().count == 1)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }

    @Test func emptyOrChangedQueueNeverDispatchesSave() async throws {
        for empty in [true, false] {
            let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
            let command = try SonosPlaylistMutation.save(title: "Keep", in: managedZone())
            if empty {
                await transport.changeQueue(total: 0)
                await #expect(throws: ContractError.self) { try await session.prepare(command) }
            } else {
                let review = try await session.prepare(command)
                await transport.changeQueue(revision: "changed-with-same-count")
                await #expect(throws: ContractError.self) { try await session.execute(review) }
            }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func newEqualTitleAfterReviewRequiresAnotherReview() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.save(title: "Evening", in: managedZone()))
        await transport.replace([managedPlaylist(), managedPlaylist("Evening", id: "SQ:9")])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: ["S1", "S2"]) func renameAndRemoveKeepHouseholdAndExactNativeIdentity(_ household: String) async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let before = household == "S1" ? managedPlaylist() : managedPlaylist("Other household")
        let rename = try await session.prepare(SonosPlaylistMutation.rename(before, title: "After & café", in: managedZone(household)))
        #expect(rename.trackCount == 3)
        let renamed = try await session.execute(rename)
        #expect(renamed.changed && renamed.playlist.id == before.id)
        let remove = try await session.prepare(SonosPlaylistMutation.remove(renamed.playlist, in: managedZone(household)))
        let removed = try await session.execute(remove)
        #expect(removed.changed && removed.playlist == renamed.playlist)
        let writes = await transport.writes()
        #expect(writes.count == 2)
        #expect(try managedFields(writes[0]) == ["zone": .string("Room + &@\(household)"), "playlist": .string("SQ:7"),
            "expected_revision": .string("saved-1"), "title": .string("After & café")])
        #expect(try managedFields(writes[1]) == ["zone": .string("Room + &@\(household)"), "playlist": .string("SQ:7"),
            "expected_revision": .string("saved-1")])
        #expect(await transport.catalogs[household]?.isEmpty == true)
        #expect(await transport.catalogs[household == "S1" ? "S2" : "S1"]?.count == 1)
    }

    @Test func unchangedRenameAndLoggingFailureRemainConfirmedWithoutReplay() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        await transport.setLogged(false)
        let review = try await session.prepare(SonosPlaylistMutation.rename(managedPlaylist(), title: "Evening", in: managedZone()))
        let receipt = try await session.execute(review)
        #expect(!receipt.changed && !receipt.logged && receipt.detail.contains("Do not repeat"))
        #expect(await transport.writes().count == 1)
    }

    @Test(arguments: ["contents", "title", "missing", "group"]) func changedCatalogOrMembershipNeverDispatchesDeletion(_ change: String) async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.remove(managedPlaylist(), in: managedZone()))
        switch change {
        case "contents": await transport.changeSaved(revision: "saved-2")
        case "title": await transport.replace([managedPlaylist("Renamed elsewhere")])
        case "missing": await transport.replace([])
        default: await transport.regroup([managedZone(members: ["Room + &", "Hall", "Guest"]), managedZone("S2")])
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }

    @Test func emptySavedPlaylistCanStillBeDeleted() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        await transport.changeSaved(total: 0)
        let review = try await session.prepare(SonosPlaylistMutation.remove(managedPlaylist(), in: managedZone()))
        #expect(review.trackCount == 0)
        #expect(try await session.execute(review).changed)
        #expect(await transport.writes().count == 1)
    }

    @Test func topologyIsCheckedAfterSlowCatalogReadBeforePresentingReview() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        await transport.hold("GET", "/sonos/playlists/contents")
        let task = Task { try await session.prepare(SonosPlaylistMutation.remove(managedPlaylist(), in: managedZone())) }
        try await managedWait { await transport.blocked }
        await transport.regroup([managedZone(members: ["Room + &", "Hall", "Guest"]), managedZone("S2")])
        await transport.release()
        await #expect(throws: ContractError.self) { try await task.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func cancellationDuringPreparationCannotCreateReviewOrWrite() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        await transport.hold("GET", "/sonos/playlists")
        let task = Task { try await session.prepare(SonosPlaylistMutation.save(title: "Keep", in: managedZone())) }
        try await managedWait { await transport.blocked }
        await session.cancel(); task.cancel(); await transport.release()
        await #expect(throws: (any Error).self) { try await task.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func sharedActorPreventsConcurrentPlaybackAndConsumesMutationOnce() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.save(title: "Keep", in: managedZone()))
        await transport.hold("GET", "/sonos/playlists")
        let first = Task { try await session.execute(review) }
        try await managedWait { await transport.blocked }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.prepare(NativeCommand.pause, in: managedZone()) }
        await transport.release(); _ = try await first.value
        #expect(await transport.writes().count == 1)
    }

    @Test(arguments: ["lost", "cancelled", "malformed", "wrongIdentity", "oldIdentity"]) func uncertainSaveNeverRetries(_ fault: String) async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.save(title: "Keep", in: managedZone()))
        switch fault {
        case "lost": await transport.loseReply()
        case "cancelled": await transport.cancelReply()
        case "malformed": await transport.overrideWrite(HTTPReply(data: Data("{}".utf8), status: 200))
        default:
            await transport.overrideWrite(try managedJSON([
                "done": JSONValue.string("Saved"), "zone": .string(fault == "wrongIdentity" ? "Other room" : "Room + &"),
                "playlist": .object(["id": .string(fault == "oldIdentity" ? "SQ:7" : "SQ:8"), "title": .string("Keep")]),
                "logged": .bool(true)]))
        }
        do { _ = try await session.execute(review); Issue.record("Uncertain save was reported as confirmed") }
        catch { if case ClientError.uncertain = error {} else { Issue.record("Expected uncertainty: \(error)") } }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        #expect(await transport.catalogs["S1"]?.count == 2)
    }

    @Test func renamedReceiptMustMatchOperationIdentityAndChangedFlag() async throws {
        let transport = PlaylistManagementTransport(), session = try CommandReviewSession(client: managedClient(transport))
        let review = try await session.prepare(SonosPlaylistMutation.rename(managedPlaylist(), title: "After", in: managedZone()))
        await transport.overrideWrite(try managedJSON([
            "done": JSONValue.string("Changed"), "zone": .string("Room + &"), "operation": .string("remove"),
            "playlist": .object(["id": .string("SQ:7"), "title": .string("After")]), "changed": .bool(true), "logged": .bool(true)]))
        do { _ = try await session.execute(review); Issue.record("Mismatched operation was accepted") }
        catch { if case ClientError.uncertain = error {} else { Issue.record("Expected uncertainty: \(error)") } }
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func nativeModelUsesSharedLaneAndChangingHouseholdRetiresUnsentReview() async throws {
        let transport = PlaylistManagementTransport(), model = try RemoteModel(client: managedClient(transport))
        model.resume()
        defer { model.stop() }
        try await managedWait { await MainActor.run { model.canPrepare && !model.panelLoading && model.queue != nil } }
        model.openPlaylistSave(); model.playlistManagement.editTitle("Keep"); model.playlistManagement.prepare()
        try await managedWait { await MainActor.run { model.playlistManagement.review != nil && model.playlistManagement.phase == .idle } }
        #expect(!model.canPrepare && !model.canSchedule)
        model.prepare(.pause)
        #expect(model.review == nil)
        model.select(RoomID(household: "S2", room: "Room + &"))
        try await managedWait { await MainActor.run { !model.playlistManagement.isEngaged } }
        #expect(model.playlistManagement.review == nil && !model.playlistManagement.isOpen)
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func uncertainReceiptSurvivesSelectionAndRecoveryReadsOriginalRegroupedHousehold() async throws {
        let transport = PlaylistManagementTransport(), workflow = try managedWorkflow(transport)
        #expect(workflow.openSave(in: managedZone()))
        workflow.editTitle("Evening"); workflow.prepare()
        try await managedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await managedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(workflow.result?.kind == .uncertain && workflow.needsRefresh)
        workflow.close()
        #expect(workflow.openSave(in: managedZone("S2")))
        #expect(workflow.operation == nil && workflow.result?.command.zone.household == "S1")
        await transport.regroup([managedZone(coordinator: "Hall"), managedZone("S2")])
        workflow.checkCatalog()
        try await managedWait { await MainActor.run { workflow.recovery != nil && !workflow.isEngaged } }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        #expect(workflow.recovery?.target == managedZone().id)
        #expect(workflow.recovery?.catalog.map(\.id) == ["SQ:7", "SQ:8"])
        #expect(workflow.recovery?.detail.contains("does not prove") == true)
        let last = try #require(await transport.requests.last(where: { $0.url?.path == "/sonos/playlists" }))
        #expect(URLComponents(url: last.url!, resolvingAgainstBaseURL: false)?.queryItems == [URLQueryItem(name: "zone", value: "Hall@S1")])
        #expect(await transport.writes().count == 1)
        workflow.stop()
    }

    @Test @MainActor func failedOrCancelledRecoveryCannotClearUncertaintyOrLoseReceipt() async throws {
        let transport = PlaylistManagementTransport(), workflow = try managedWorkflow(transport)
        workflow.openRemove(managedPlaylist(), in: managedZone()); workflow.prepare()
        try await managedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await managedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        let resultID = workflow.result?.id
        await transport.fail("/sonos/playlists"); workflow.checkCatalog()
        try await managedWait { await MainActor.run { workflow.problem != nil && !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.result?.id == resultID)
        workflow.dismissResult(); #expect(workflow.result?.id == resultID)
        await transport.fail(nil); await transport.hold("GET", "/sonos/playlists")
        workflow.checkCatalog(); try await managedWait { await transport.blocked }
        workflow.close(); await transport.release()
        try await managedWait { await MainActor.run { !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.recovery == nil)
        workflow.openResult(); workflow.checkCatalog()
        try await managedWait { await MainActor.run { workflow.recovery != nil && !workflow.isEngaged } }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        #expect(workflow.recovery?.detail.contains("not currently listed") == true)
        #expect(await transport.writes().count == 1)
        workflow.stop()
    }

    @Test @MainActor func absentOriginalRoomNeverSubstitutesSameNamedOtherHousehold() async throws {
        let transport = PlaylistManagementTransport(), workflow = try managedWorkflow(transport)
        workflow.openRename(managedPlaylist(), in: managedZone()); workflow.editTitle("After"); workflow.prepare()
        try await managedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await managedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        await transport.regroup([managedZone("S2")])
        let reads = await transport.requests.count
        workflow.checkCatalog()
        try await managedWait { await MainActor.run { workflow.problem != nil && !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.recovery == nil)
        let later = Array(await transport.requests.dropFirst(reads))
        #expect(later.allSatisfy { $0.url?.path == "/zones" })
        #expect(await transport.writes().count == 1)
        workflow.stop()
    }

    @Test @MainActor func lateConfirmedReceiptStaysOriginalWhileSuspended() async throws {
        let transport = PlaylistManagementTransport(), workflow = try managedWorkflow(transport)
        workflow.openSave(in: managedZone()); workflow.editTitle("Keep"); workflow.prepare()
        try await managedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.hold("POST", "/sonos/playlists/save"); workflow.confirm(review.id)
        try await managedWait { await transport.blocked }
        workflow.suspend(); #expect(workflow.isEngaged)
        await transport.release()
        try await managedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(!workflow.active && workflow.result?.kind == .confirmed)
        #expect(workflow.result?.command.zone.household == "S1" && workflow.result?.receipt?.playlist.id == "SQ:8")
        #expect(await transport.writes().count == 1)
        workflow.resume(); #expect(workflow.openResult())
        #expect(await transport.writes().count == 1)
        workflow.stop()
    }
}
