import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

func favoriteManagedZone(_ household: String = "S1", coordinator: String = "Room + &", members: [String] = ["Room + &", "Hall"]) -> Zone {
    Zone(household: household, coordinatorRoom: coordinator, members: members, transportState: "playing")
}
func favoriteManagedItem(_ title: String = "Evening", id: String = "FV:2/7", kind: String = "track", description: String? = "Native description") -> Favorite {
    Favorite(id: id, title: title, kind: kind, description: description)
}
func favoriteManagedJSON<T: Encodable>(_ value: T) throws -> HTTPReply {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    return HTTPReply(data: try encoder.encode(value), status: 200)
}
func favoriteManagedFields(_ request: URLRequest) throws -> [String: JSONValue] {
    try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody ?? Data())
}
private func favoriteManagedString(_ value: JSONValue?) -> String? {
    if case .string(let string) = value { return string }; return nil
}
func favoriteManagedValue(_ favorite: Favorite) -> JSONValue {
    .object(["id": .string(favorite.id), "title": .string(favorite.title), "kind": .string(favorite.kind),
             "description": favorite.description.map(JSONValue.string) ?? .null])
}

/// Only the fixture's native favorites mutate. Production command/session and
/// workflow code own observations, the one-use write lane and late receipts.
actor NativeFavoriteManagementTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var topology = [favoriteManagedZone(), favoriteManagedZone("S2")]
    var catalogs = ["S1": [favoriteManagedItem()], "S2": [favoriteManagedItem("Other household")]]
    var source = favoriteManagedItem("Current track", id: "", description: "Source artist")
    var sourceRevision = "source-1"
    var catalogRevisions = ["S1": "catalog-1", "S2": "other-catalog-1"]
    var logged = true
    var lost = false
    var cancelled = false
    var denyWrite = false
    var writeOverride: HTTPReply?
    var readOverrides: [String: HTTPReply] = [:]
    var failRead: String?
    var blocked = false
    private var holdKey: String?
    private var waiter: CheckedContinuation<Void, Never>?

    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func changeSource(_ value: Favorite? = nil, revision: String? = nil) {
        if let value { source = value }; if let revision { sourceRevision = revision }
    }
    func changeCatalogRevision(_ value: String, household: String = "S1") { catalogRevisions[household] = value }
    func replace(_ values: [Favorite], household: String = "S1") { catalogs[household] = values }
    func regroup(_ value: [Zone]) { topology = value }
    func setLogged(_ value: Bool) { logged = value }
    func loseReply() { lost = true }
    func cancelReply() { cancelled = true }
    func denyMutation() { denyWrite = true }
    func overrideWrite(_ value: HTTPReply) { writeOverride = value }
    func overrideRead(_ path: String, reply: HTTPReply?) { readOverrides[path] = reply }
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
            let zone = topology.first(where: { $0.household == household }) ?? favoriteManagedZone(household)
            if let value = readOverrides[path] { response = value }
            else {
                switch path {
                case "/zones": response = try favoriteManagedJSON(topology)
                case "/favorites": response = try favoriteManagedJSON(catalogs[household] ?? [])
                case "/favorites/current":
                    response = try favoriteManagedJSON(["zone": JSONValue.string(zone.coordinatorRoom),
                        "favorite": favoriteManagedValue(source), "revision": .string(sourceRevision)])
                case "/favorites/inspect":
                    let id = query.first(where: { $0.name == "favorite" })?.value
                    guard let item = catalogs[household]?.first(where: { $0.id == id }) else {
                        return HTTPReply(data: Data(#"{"code":"NO_MATCH","detail":"No native favorite"}"#.utf8), status: 404)
                    }
                    response = try favoriteManagedJSON(["zone": JSONValue.string(zone.coordinatorRoom),
                        "favorite": favoriteManagedValue(item), "revision": .string(catalogRevisions[household]!)])
                default:
                    if path.hasSuffix("/state") {
                        response = try favoriteManagedJSON(ZoneState(zone: zone, transportState: "playing", volume: 20,
                            mute: false, groupVolume: 20, track: .init(uri: "https://media.example.test/current",
                                title: "Playing source", creator: nil, album: nil, durationSecs: nil,
                                positionSecs: nil, queuePosition: nil), shuffle: false, repeat: "off"))
                    } else if path.hasSuffix("/queue") {
                        response = try favoriteManagedJSON(["zone": JSONValue.string(zone.coordinatorRoom),
                            "total": .integer(1), "revision": .string("unrelated-queue-revision"), "playing": .integer(1),
                            "items": .array([.object(["position": .integer(1), "title": .string("Retained queue track"),
                                                     "uri": .string("https://media.example.test/retained")])])])
                    } else { throw ContractError.invalid("Unexpected favorite management read: \(path)") }
                }
            }
        } else {
            if denyWrite {
                return HTTPReply(data: Data(#"{"code":"POLICY_DENIED","detail":"Favorite changes are denied","hint":"Read access remains available"}"#.utf8), status: 403)
            }
            let fields = try favoriteManagedFields(request)
            let address = favoriteManagedString(fields["zone"]) ?? ""
            let household = address.hasSuffix("@S2") ? "S2" : "S1"
            let coordinator = topology.first(where: { $0.household == household })?.coordinatorRoom ?? "Room + &"
            let original = catalogs[household] ?? []
            let item: Favorite
            let operation: String
            var changed = true
            switch path {
            case "/favorites/save":
                guard favoriteManagedString(fields["expected_revision"]) == sourceRevision else { throw ClientError.unreadable }
                item = favoriteManagedItem(favoriteManagedString(fields["title"]) ?? "", id: "FV:2/8",
                    kind: source.kind, description: source.description.flatMap {
                        $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : $0
                    })
                catalogs[household, default: []].append(item); operation = "save"
            case "/favorites/rename", "/favorites/remove":
                guard favoriteManagedString(fields["expected_revision"]) == catalogRevisions[household],
                      let index = original.firstIndex(where: { $0.id == favoriteManagedString(fields["favorite"]) }) else {
                    throw ClientError.unreadable
                }
                if path.hasSuffix("/rename") {
                    item = favoriteManagedItem(favoriteManagedString(fields["title"]) ?? "", id: original[index].id,
                                               kind: original[index].kind, description: original[index].description)
                    changed = !item.title.utf8.elementsEqual(original[index].title.utf8)
                    catalogs[household]![index] = item; operation = "rename"
                } else { item = original[index]; catalogs[household]!.remove(at: index); operation = "remove" }
            case "/pause":
                return try favoriteManagedJSON(["done": JSONValue.string("Paused"), "changed": .bool(true)])
            default: throw ContractError.invalid("Unexpected favorite management write: \(path)")
            }
            if changed { catalogRevisions[household, default: "catalog"] += ":next" }
            response = try writeOverride ?? favoriteManagedJSON(["done": JSONValue.string("Favorite catalog confirmed"),
                "zone": .string(coordinator), "favorite": favoriteManagedValue(item), "operation": .string(operation),
                "changed": .bool(changed), "logged": .bool(logged)])
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
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(pair.continuation) }
        return pair.stream
    }
    private func register(_ output: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        streams.append(output); output.yield(.connected)
    }
    func catalogEvent(_ household: String = "S1", favorites: Bool = true, playlists: Bool = false, count: Int = 1) {
        let fields: [String: Any] = ["household": household, "favorites": favorites, "playlists": playlists]
        let data = try! JSONSerialization.data(withJSONObject: fields)
        for _ in 0..<count {
            streams.last?.yield(.event(ServerEvent(id: nil, kind: "catalog.changed", data: String(decoding: data, as: UTF8.self))))
        }
    }
}

func favoriteManagedClient(_ transport: NativeFavoriteManagementTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
func favoriteManagedWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Favorite management test did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@Suite("Native favorite management") struct NativeFavoriteManagementTests {
    @Test func titleAndNativeIdentityValidationPrecedesReads() throws {
        #expect(try FavoriteMutation.validatedTitle("\u{85}  Dvořák & <friends>  \u{a0}") == "Dvořák & <friends>")
        #expect(try FavoriteMutation.validatedTitle(String(repeating: "é", count: 511)).utf8.count == 1_022)
        #expect(try FavoriteMutation.validatedTitle("\u{feff}Title") == "\u{feff}Title")
        for title in ["", " \n\t ", "A\0B", "A\nB", String(repeating: "é", count: 512)] {
            #expect(throws: ContractError.self) { try FavoriteMutation.save(title: title, in: favoriteManagedZone()) }
        }
        for id in ["", "Evening", "7", "SQ:7", "FV:2/", "FV:2/-1", "FV:2/7/8", " FV:2/7", "FV:2/٧"] {
            #expect(throws: ClientError.self) { try FavoriteMutation.remove(favoriteManagedItem(id: id), in: favoriteManagedZone()) }
        }
    }

    @Test(arguments: ["track", "stream"]) func saveUsesCurrentNativeSourceAndCreatesDistinctSameTitle(_ kind: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        await transport.changeSource(favoriteManagedItem(kind == "stream" ? "Current station" : "Current track", id: "", kind: kind))
        let review = try await session.prepare(FavoriteMutation.save(title: " Evening ", in: favoriteManagedZone()))
        #expect(review.favorite.kind == kind && review.favorite.id.isEmpty && review.duplicateTitles == 1)
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.favorite.id == "FV:2/8" && receipt.favorite.title == "Evening")
        #expect(receipt.favorite.kind == kind && receipt.changed && receipt.logged)
        let write = try #require(await transport.writes().first)
        #expect(write.url?.path == "/favorites/save")
        #expect(try favoriteManagedFields(write) == ["zone": .string("Room + &@S1"), "title": .string("Evening"),
                                                     "expected_revision": .string("source-1")])
        #expect(await transport.catalogs["S1"]?.map(\.id) == ["FV:2/7", "FV:2/8"])
        #expect(await transport.catalogs["S2"] == [favoriteManagedItem("Other household")])
        let reads = await transport.requests.filter { $0.httpMethod == "GET" }
        #expect(reads.allSatisfy { ["/zones", "/favorites", "/favorites/current"].contains($0.url!.path) })
        #expect(reads.filter { $0.url?.path == "/favorites/current" }.count == 2)
        #expect(reads.filter { $0.url?.path == "/favorites/current" }.allSatisfy {
            URLComponents(url: $0.url!, resolvingAgainstBaseURL: false)?.queryItems == [URLQueryItem(name: "zone", value: "Room + &@S1")]
        })
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }

    @Test(arguments: ["S1", "S2"]) func renameAndRemoveUseExactFavoriteAndIndependentCatalogRevisions(_ household: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let before = household == "S1" ? favoriteManagedItem() : favoriteManagedItem("Other household")
        let revision = household == "S1" ? "catalog-1" : "other-catalog-1"
        let rename = try await session.prepare(FavoriteMutation.rename(before, title: "After & café", in: favoriteManagedZone(household)))
        // The current source can advance independently of a saved catalog edit.
        await transport.changeSource(revision: "another-current-source")
        let renamed = try await session.execute(rename)
        #expect(renamed.changed && renamed.favorite.id == before.id && renamed.favorite.description == before.description)
        let remove = try await session.prepare(FavoriteMutation.remove(renamed.favorite, in: favoriteManagedZone(household)))
        let removed = try await session.execute(remove)
        #expect(removed.changed && removed.favorite == renamed.favorite)
        let writes = await transport.writes()
        #expect(writes.count == 2)
        #expect(try favoriteManagedFields(writes[0]) == ["zone": .string("Room + &@\(household)"), "favorite": .string("FV:2/7"),
            "expected_revision": .string(revision), "title": .string("After & café")])
        #expect(try favoriteManagedFields(writes[1]) == ["zone": .string("Room + &@\(household)"), "favorite": .string("FV:2/7"),
            "expected_revision": .string(revision + ":next")])
        #expect(await transport.requests.allSatisfy { $0.url?.path != "/favorites/current" })
        #expect(await transport.catalogs[household]?.isEmpty == true)
        #expect(await transport.catalogs[household == "S1" ? "S2" : "S1"]?.count == 1)
    }

    @Test(arguments: [" \t ", " Artist & friends "])
    func saveReceiptAcceptsOnlyTheDaemonsBlankDescriptionNormalization(_ description: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        await transport.changeSource(favoriteManagedItem("Current track", id: "", description: description))
        let review = try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone()))
        let receipt = try await session.execute(review)
        #expect(receipt.favorite.description == (description == " \t " ? nil : description))
        #expect(await transport.writes().count == 1)
    }

    @Test func unchangedRenameAndLoggingFailureAreConfirmedWithoutReplay() async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        await transport.setLogged(false)
        let review = try await session.prepare(FavoriteMutation.rename(favoriteManagedItem(), title: "Evening", in: favoriteManagedZone()))
        let receipt = try await session.execute(review)
        #expect(!receipt.changed && !receipt.logged && receipt.detail.contains("Do not repeat"))
        #expect(await transport.writes().count == 1)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
    }

    @Test(arguments: ["revision", "title", "kind", "description", "group", "duplicateTitle"])
    func changedCurrentSourceOrReviewContextPreventsSaving(_ change: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.save(title: "Evening", in: favoriteManagedZone()))
        switch change {
        case "revision": await transport.changeSource(revision: "new-source")
        case "title": await transport.changeSource(favoriteManagedItem("Different track", id: "", description: "Source artist"))
        case "kind": await transport.changeSource(favoriteManagedItem("Current track", id: "", kind: "stream", description: "Source artist"))
        case "description": await transport.changeSource(favoriteManagedItem("Current track", id: "", description: "Different artist"))
        case "group": await transport.regroup([favoriteManagedZone(members: ["Room + &", "Hall", "Guest"]), favoriteManagedZone("S2")])
        default: await transport.replace([favoriteManagedItem(), favoriteManagedItem(id: "FV:2/9")])
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: ["revision", "title", "kind", "description", "missing", "duplicateID"])
    func changedSavedFavoriteCannotBeRenamedOrRemoved(_ change: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.remove(favoriteManagedItem(), in: favoriteManagedZone()))
        switch change {
        case "revision": await transport.changeCatalogRevision("new-catalog")
        case "title": await transport.replace([favoriteManagedItem("Different title")])
        case "kind": await transport.replace([favoriteManagedItem(kind: "stream")])
        case "description": await transport.replace([favoriteManagedItem(description: "Changed metadata")])
        case "missing": await transport.replace([])
        default: await transport.replace([favoriteManagedItem(), favoriteManagedItem("Same native identity")])
        }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: ["container", "savedID", "missingRevision", "controlRevision", "largeRevision", "wrongZone"])
    func ineligibleOrUnboundCurrentObservationCannotCreateSaveReview(_ fault: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let item = favoriteManagedItem("Current", id: fault == "savedID" ? "FV:2/8" : "", kind: fault == "container" ? "container" : "track")
        let revision = fault == "missingRevision" ? "" : fault == "controlRevision" ? "a\nb" : fault == "largeRevision" ? String(repeating: "r", count: 4_097) : "source-1"
        await transport.overrideRead("/favorites/current", reply: try favoriteManagedJSON([
            "zone": JSONValue.string(fault == "wrongZone" ? "Other room" : "Room + &"),
            "favorite": favoriteManagedValue(item), "revision": .string(revision)]))
        await #expect(throws: (any Error).self) {
            try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone()))
        }
        #expect(await transport.writes().isEmpty)
    }

    @Test func slowObservationRechecksTopologyBeforePresentingReview() async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        await transport.hold("GET", "/favorites/inspect")
        let work = Task { try await session.prepare(FavoriteMutation.remove(favoriteManagedItem(), in: favoriteManagedZone())) }
        try await favoriteManagedWait { await transport.blocked }
        await transport.regroup([favoriteManagedZone(members: ["Room + &", "Hall", "Guest"]), favoriteManagedZone("S2")])
        await transport.release()
        await #expect(throws: ContractError.self) { try await work.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func cancellationDuringPreparationCannotReviveAReview() async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        await transport.hold("GET", "/favorites/current")
        let work = Task { try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone())) }
        try await favoriteManagedWait { await transport.blocked }
        await session.cancel(); work.cancel(); await transport.release()
        await #expect(throws: (any Error).self) { try await work.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func sharedActorFencesPlaybackPlaylistAndDuplicateFavoriteConfirmations() async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone()))
        await transport.hold("GET", "/favorites/current")
        let first = Task { try await session.execute(review) }
        try await favoriteManagedWait { await transport.blocked }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.prepare(NativeCommand.pause, in: favoriteManagedZone()) }
        await #expect(throws: ContractError.self) {
            try await session.prepare(SonosPlaylistMutation.save(title: "Queue", in: favoriteManagedZone()))
        }
        await transport.release(); _ = try await first.value
        #expect(await transport.writes().count == 1)
    }

    @Test(arguments: ["lost", "cancelled", "malformed", "wrongZone", "oldIdentity", "wrongOperation", "wrongKind", "wrongDescription", "unchanged"])
    func unconfirmedSaveNeverRetriesOrClaimsSuccess(_ fault: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone()))
        switch fault {
        case "lost": await transport.loseReply()
        case "cancelled": await transport.cancelReply()
        case "malformed": await transport.overrideWrite(HTTPReply(data: Data("{}".utf8), status: 200))
        default:
            let item = favoriteManagedItem("Keep", id: fault == "oldIdentity" ? "FV:2/7" : "FV:2/8",
                kind: fault == "wrongKind" ? "stream" : "track", description: fault == "wrongDescription" ? "Other artist" : "Source artist")
            await transport.overrideWrite(try favoriteManagedJSON([
                "done": JSONValue.string("Saved"), "zone": .string(fault == "wrongZone" ? "Other room" : "Room + &"),
                "favorite": favoriteManagedValue(item), "operation": .string(fault == "wrongOperation" ? "rename" : "save"),
                "changed": .bool(fault != "unchanged"), "logged": .bool(true)]))
        }
        do { _ = try await session.execute(review); Issue.record("Unconfirmed favorite was reported as saved") }
        catch { if case ClientError.uncertain = error {} else { Issue.record("Expected uncertainty: \(error)") } }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        #expect(await transport.catalogs["S1"]?.count == 2)
    }

    @Test(arguments: ["identity", "title", "kind", "description", "operation", "unchanged"])
    func editedReceiptMustMatchTheExactReviewedFavorite(_ fault: String) async throws {
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.rename(favoriteManagedItem(), title: "After", in: favoriteManagedZone()))
        let item = favoriteManagedItem(fault == "title" ? "Another title" : "After",
            id: fault == "identity" ? "FV:2/9" : "FV:2/7", kind: fault == "kind" ? "stream" : "track",
            description: fault == "description" ? "Other source" : "Native description")
        await transport.overrideWrite(try favoriteManagedJSON([
            "done": JSONValue.string("Renamed"), "zone": .string("Room + &"), "favorite": favoriteManagedValue(item),
            "operation": .string(fault == "operation" ? "remove" : "rename"), "changed": .bool(fault != "unchanged"), "logged": .bool(true)]))
        do { _ = try await session.execute(review); Issue.record("An unrelated favorite receipt was accepted") }
        catch { if case ClientError.uncertain = error {} else { Issue.record("Expected uncertainty: \(error)") } }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        #expect(await transport.catalogs["S1"]?.first?.title == "After")
    }

    @Test func policyErrorsBeforeAndAfterDispatchNeverReplay() async throws {
        let denied = HTTPReply(data: Data(#"{"code":"POLICY_DENIED","detail":"Favorite changes are denied"}"#.utf8), status: 403)
        let before = NativeFavoriteManagementTransport(), first = try CommandReviewSession(client: favoriteManagedClient(before))
        await before.overrideRead("/favorites/current", reply: denied)
        await #expect(throws: APIProblem.self) { try await first.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone())) }
        #expect(await before.writes().isEmpty)
        let transport = NativeFavoriteManagementTransport(), session = try CommandReviewSession(client: favoriteManagedClient(transport))
        let review = try await session.prepare(FavoriteMutation.save(title: "Keep", in: favoriteManagedZone()))
        await transport.denyMutation()
        do { _ = try await session.execute(review); Issue.record("Policy denial was accepted as success") }
        catch { #expect(error.localizedDescription.contains("Favorite changes are denied")) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        #expect(await transport.catalogs["S1"]?.count == 1)
    }
}
