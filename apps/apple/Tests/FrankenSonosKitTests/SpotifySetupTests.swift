import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let testRedirect = "http://127.0.0.1:8099/auth/spotify/callback"
private let spotifyFixtureState = "SYNTHETIC-state"
private let testCode = "SYNTHETIC-code"
private func jsonReply(_ object: Any, status: Int = 200) throws -> HTTPReply {
    HTTPReply(data: try JSONSerialization.data(withJSONObject: object), status: status)
}
private func consentObject(redirect: String = testRedirect) -> [String: String] {
    var c = URLComponents(string: "https://accounts.spotify.com/authorize")!
    c.queryItems = [URLQueryItem(name: "client_id", value: "SYNTHETIC"), URLQueryItem(name: "response_type", value: "code"),
                   URLQueryItem(name: "redirect_uri", value: redirect), URLQueryItem(name: "state", value: spotifyFixtureState),
                   URLQueryItem(name: "scope", value: "user-library-read user-top-read"),
                   URLQueryItem(name: "code_challenge_method", value: "S256"),
                   URLQueryItem(name: "code_challenge", value: String(repeating: "A", count: 43))]
    c.percentEncodedQuery = c.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
    return ["url": c.string!, "redirect_uri": redirect]
}
private func consent(_ object: [String: String] = consentObject()) throws -> SpotifyConsent {
    try DaemonClient.decode(SpotifyConsent.self, from: jsonReply(object).data)
}
private func callback(code: String = testCode) -> String {
    var c = URLComponents(string: testRedirect)!
    c.queryItems = [URLQueryItem(name: "code", value: code), URLQueryItem(name: "state", value: spotifyFixtureState)]
    return c.string!
}

private actor SpotifyStub: DaemonTransport {
    var signedIn = false, pending = false, running = false
    var imported = false, partial = false
    var redirect = testRedirect
    var requests: [URLRequest] = []
    var overrides: [String: HTTPReply] = [:]
    var losePosts = false
    var blocked = false
    var inFlight = 0, maximumInFlight = 0
    private var gatePath: String?
    private var gate: CheckedContinuation<Void, Never>?

    func setAuth(signed: Bool, pending: Bool) { signedIn = signed; self.pending = pending }
    func setRunning(_ running: Bool) { self.running = running }
    func finish(partial: Bool = false) { running = false; imported = true; self.partial = partial }
    func setRedirect(_ value: String) { redirect = value }
    func override(_ key: String, _ reply: HTTPReply?) { overrides[key] = reply }
    func loseWrites() { losePosts = true }
    func hold(_ path: String) { gatePath = path; blocked = false }
    func release() { let saved = gate; gate = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func count(_ path: String) -> Int { requests.filter { $0.url?.path == path }.count }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request); inFlight += 1; maximumInFlight = max(maximumInFlight, inFlight)
        defer { inFlight -= 1 }
        let key = (request.httpMethod ?? "GET") + " " + request.url!.path
        let reply: HTTPReply
        if let overridden = overrides[key] { reply = overridden }
        else {
            switch key {
            case "GET /auth/spotify": reply = try auth()
            case "GET /dj/sync": reply = try sync()
            case "POST /auth/spotify/begin": pending = true; reply = try jsonReply(consentObject(redirect: redirect))
            case "POST /auth/spotify/complete": signedIn = true; pending = false; reply = try auth()
            case "POST /dj/sync": running = true; reply = try sync()
            default: throw ContractError.invalid("Unexpected route")
            }
        }
        if gatePath == key {
            gatePath = nil; blocked = true
            // Deliberately ignore cancellation until the test releases I/O.
            await withCheckedContinuation { gate = $0 }
        }
        if request.httpMethod == "POST" && losePosts {
            throw NSError(domain: "Synthetic callback echo", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: callback()])
        }
        return reply
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    private func auth() throws -> HTTPReply {
        try jsonReply(["signed_in": signedIn, "pending": pending, "redirect_uri": redirect])
    }
    private func sync() throws -> HTTPReply {
        var object: [String: Any] = ["done": running ? "Reading library" : "Idle library", "running": running]
        if imported {
            var last: [String: Any] = ["at": 1_790_000_000, "tracks": 100, "candidates": 90, "classical": 10, "retired": 3]
            if partial { last["genre_artists_pending"] = 4; last["notes"] = ["Artist lookup rate limited"] }
            object["last"] = last
        }
        return try jsonReply(object)
    }
}
private func client(_ stub: SpotifyStub) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: stub)
}
private func gate(_ stub: SpotifyStub) async throws {
    let until = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await stub.blocked) {
        guard ContinuousClock.now < until else { throw ContractError.invalid("Gate not reached") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func eventually(_ check: () -> Bool) async throws {
    let until = ContinuousClock.now.advanced(by: .seconds(3))
    while !check() {
        guard ContinuousClock.now < until else { throw ContractError.invalid("Model did not settle") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func ready(_ stub: SpotifyStub, poll: Duration = .milliseconds(80)) async throws -> SpotifySetupModel {
    let model = try SpotifySetupModel(client: client(stub), timing: .init(poll: poll, maximumRetry: .milliseconds(320)))
    model.resume()
    try await eventually { model.activity == .idle && model.observedAt != nil }
    return model
}

@Test func spotifyConsentAcceptsOnlyTheExistingReadOnlyPKCEFlow() throws {
    let c = try consent()
    #expect(try c.authorizationURL().host == "accounts.spotify.com")
    #expect(!String(describing: c).contains(spotifyFixtureState))
    #expect(!String(reflecting: c).contains(spotifyFixtureState))
    var object = consentObject()
    object["url"] = object["url"]!.replacingOccurrences(of: "%20", with: "+")
    #expect(try consent(object).authorizationURL().host == "accounts.spotify.com")
}

@Test func spotifyConsentRejectsUnsafeOriginsGrantsAndDuplicatedFields() throws {
    let base = consentObject()
    var variants = [String]()
    for (from, to) in [("https://accounts.spotify.com", "https://accounts.spotify.com.evil.test"),
                       ("https://accounts.spotify.com", "http://accounts.spotify.com"),
                       ("https://accounts.spotify.com", "https://user@accounts.spotify.com"),
                       ("/authorize?", "/api/token?"), ("response_type=code", "response_type=token"),
                       ("code_challenge_method=S256", "code_challenge_method=plain"),
                       ("user-library-read", "user-modify-playback-state"), ("SYNTHETIC", "") ] {
        variants.append(base["url"]!.replacingOccurrences(of: from, with: to))
    }
    variants += [base["url"]! + "&state=other", base["url"]! + "#token=secret", base["url"]! + "&scope=user-library-read", base["url"]! + "&secret=%XX"]
    for url in variants { #expect(throws: (any Error).self) { try consent(["url": url, "redirect_uri": testRedirect]).authorizationURL() } }
}

@Test func spotifyCallbackBindsStateOriginAndExactPathWithoutVisitingIt() throws {
    let c = try consent()
    #expect(try c.callback(callback()) == callback())
    let malformed = ["?code=x&state=" + spotifyFixtureState, callback().replacingOccurrences(of: spotifyFixtureState, with: "other"),
                     callback().replacingOccurrences(of: "127.0.0.1", with: "evil.test"),
                     callback().replacingOccurrences(of: "/callback", with: "/different"),
                     callback() + "&code=extra", callback() + "#access_token=secret", callback() + "&error=denied",
                     callback().replacingOccurrences(of: testCode, with: "%ZZ"), callback() + "&token=secret"]
    for input in malformed {
        #expect(throws: SpotifySetupError.invalidCallback) { try c.callback(input) }
    }
}

@Test func spotifyCallbackPreservesPercentEncodedPlusAndDoesNotEchoCodes() throws {
    let c = try consent()
    let value = testRedirect + "?code=SYNTHETIC%2Bcode%2Fvalue&state=" + spotifyFixtureState
    #expect(try c.callback(value) == value)
    do { _ = try c.callback(callback() + "&state=other") }
    catch { #expect(!error.localizedDescription.contains(testCode)); #expect(!error.localizedDescription.contains(spotifyFixtureState)) }
    let redirect = "https://daemon.example.test/auth/a+b"
    #expect(try consent(consentObject(redirect: redirect)).authorizationURL().host == "accounts.spotify.com")
}

@Test func spotifySyncDecodesOmittedEmptyFieldsAndRetainsIncompleteImports() throws {
    let last: [String: Any] = ["at": 1_790_000_000, "tracks": 10, "candidates": 8, "classical": 2, "retired": 100]
    let raw = try jsonReply(["done": "Imported", "running": false, "last": last])
    let old = try DaemonClient.decode(SpotifyLibrarySync.self, from: raw.data).validated()
    #expect(old.last?.incomplete == false && old.last?.notes == nil)
    var partial = last; partial["genre_artists_pending"] = 1; partial["notes"] = ["Taste scope missing"]
    let value = try DaemonClient.decode(SpotifyLibrarySync.self, from: jsonReply(["done": "Reading", "running": true, "last": partial, "error": "Previous attempt failed"]).data).validated()
    #expect(value.running && value.last?.incomplete == true && value.error != nil)
}

@Test func spotifySyncRejectsMalformedCountsAndEmptyResponses() throws {
    for raw in ["{}", "{\"done\":\"\",\"running\":false}",
                "{\"done\":\"x\",\"running\":false,\"last\":{\"at\":0,\"tracks\":1,\"candidates\":2,\"classical\":0,\"retired\":0}}",
                "{\"done\":\"x\",\"running\":false,\"last\":{\"at\":0,\"tracks\":1,\"candidates\":1,\"classical\":2,\"retired\":0}}"] {
        #expect(throws: (any Error).self) { try DaemonClient.decode(SpotifyLibrarySync.self, from: Data(raw.utf8)).validated() }
    }
}

@Test func spotifyClientUsesExactDaemonRoutesAndDoesNotForgeIdentity() async throws {
    let stub = SpotifyStub(), client = try client(stub)
    _ = try await client.spotifySignIn(); _ = try await client.spotifyLibrarySync()
    let consent = try await client.beginSpotifySignIn()
    _ = try await client.completeSpotifySignIn(callback(), consent: consent)
    _ = try await client.startSpotifyLibrarySync()
    let writes = await stub.writes()
    #expect(writes.map { $0.url!.path } == ["/auth/spotify/begin", "/auth/spotify/complete", "/dj/sync"])
    #expect(try JSONDecoder().decode([String: String].self, from: writes[0].httpBody!) == [:])
    #expect(try JSONDecoder().decode([String: String].self, from: writes[1].httpBody!) == ["redirect": callback()])
    #expect(try JSONDecoder().decode([String: String].self, from: writes[2].httpBody!) == [:])
    for request in await stub.requests {
        #expect(request.url?.host == "daemon.example.test")
        #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
        #expect(request.value(forHTTPHeaderField: "Tailscale-User-Login") == nil)
        #expect(request.url?.query == nil)
    }
}

@Test func spotifyClientDoesNotSendAnInvalidCallback() async throws {
    let stub = SpotifyStub(), client = try client(stub)
    await #expect(throws: SpotifySetupError.invalidCallback) { try await client.completeSpotifySignIn("secret-code", consent: consent()) }
    #expect(await stub.requests.isEmpty)
}

@Test func spotifyClientWritesAreSingleShotAndRedactEveryFailedAcknowledgement() async throws {
    for mode in 0..<3 {
        let stub = SpotifyStub(), client = try client(stub)
        if mode == 0 { await stub.loseWrites() }
        if mode == 1 { await stub.override("POST /auth/spotify/complete", HTTPReply(data: Data(callback().utf8), status: 200)) }
        if mode == 2 { await stub.override("POST /auth/spotify/complete", try jsonReply(["code": "POLICY_DENIED", "detail": callback()], status: 403)) }
        do { _ = try await client.completeSpotifySignIn(callback(), consent: consent()); Issue.record("Expected uncertainty") }
        catch {
            #expect(error as? SpotifySetupError == .uncertain)
            #expect(!error.localizedDescription.contains(testCode) && !error.localizedDescription.contains(spotifyFixtureState))
        }
        #expect(await stub.writes().count == 1)
    }
}

@Test func spotifyClientRejectsContradictoryCompletionAndUnsafeBeginReceipts() async throws {
    let stub = SpotifyStub(), client = try client(stub)
    await stub.override("POST /auth/spotify/complete", try jsonReply(["signed_in": true, "pending": true, "redirect_uri": testRedirect]))
    await #expect(throws: SpotifySetupError.uncertain) { try await client.completeSpotifySignIn(callback(), consent: consent()) }
    await stub.override("POST /auth/spotify/begin", try jsonReply(["url": "https://evil.test/", "redirect_uri": testRedirect]))
    await #expect(throws: SpotifySetupError.uncertain) { try await client.beginSpotifySignIn() }
    #expect(await stub.writes().count == 2)
}

@Test @MainActor func spotifySetupQuietObservationsNeverPollOrWrite() async throws {
    let stub = SpotifyStub(), model = try await ready(stub, poll: .milliseconds(20))
    defer { model.stop() }
    let reads = await stub.requests.count
    try await Task.sleep(for: .milliseconds(90))
    #expect(await stub.requests.count == reads)
    #expect(model.canBegin && model.canSync && !model.canComplete)
    #expect(await stub.writes().isEmpty)
}

@Test @MainActor func spotifySetupConnectsSignInCompletionAndLibraryRefresh() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn(); model.beginSignIn()
    try await eventually { model.canComplete }
    #expect(model.consent != nil)
    model.completeSignIn(callback()); model.completeSignIn(callback())
    try await eventually { model.activity == .idle && model.signIn?.signedIn == true }
    #expect(model.consent == nil)
    model.syncLibrary(); model.syncLibrary()
    try await eventually { model.library?.running == true && model.activity == .idle }
    await stub.finish(partial: true)
    try await eventually { model.library?.last != nil && model.library?.running == false }
    #expect(model.library?.last?.incomplete == true)
    #expect(model.library?.last?.notes == ["Artist lookup rate limited"])
    #expect(await stub.writes().map { $0.url!.path } == ["/auth/spotify/begin", "/auth/spotify/complete", "/dj/sync"])
}

@Test @MainActor func spotifyPendingSignInRequiresExplicitReplacement() async throws {
    let stub = SpotifyStub()
    await stub.setAuth(signed: true, pending: true)
    let model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    #expect(await stub.writes().isEmpty)
    #expect(model.result?.title == "Sign-in already pending")
    model.beginSignIn(replacePending: true)
    try await eventually { model.canComplete }
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifyPreflightRefusesAReplacedAuthConfiguration() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    await stub.setRedirect("https://other.example.test/callback")
    model.beginSignIn()
    try await eventually { model.result != nil && model.activity == .idle }
    #expect(await stub.writes().isEmpty && model.consent == nil)
}

@Test @MainActor func spotifyAutomaticBrowserCallbackDoesNotAutomaticallySync() async throws {
    let stub = SpotifyStub(), model = try await ready(stub, poll: .milliseconds(20))
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    await stub.setAuth(signed: true, pending: false)
    try await eventually { model.signIn?.signedIn == true && model.consent == nil }
    #expect(await stub.writes().map { $0.url!.path } == ["/auth/spotify/begin"])
}

@Test @MainActor func spotifyInvalidPasteDoesNotConsumeTheUsableConsentLink() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    model.completeSignIn("not-a-callback-" + testCode)
    #expect(model.canComplete && model.consent != nil)
    #expect(model.result?.title == "Callback not sent")
    #expect(model.result?.detail.contains(testCode) == false)
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifySuspendDuringPreflightPreventsDispatch() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    await stub.hold("GET /auth/spotify")
    model.beginSignIn()
    try await gate(stub)
    model.suspend(); model.beginSignIn()
    await stub.release()
    try await eventually { model.activity == .idle }
    #expect(await stub.writes().isEmpty && model.consent == nil)
}

@Test @MainActor func spotifyLostCompletionStaysUncertainAfterObservedSuccess() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    await stub.loseWrites()
    model.completeSignIn(callback())
    try await eventually { model.activity == .idle && model.signIn?.signedIn == true }
    #expect(model.result?.uncertain == true && model.consent == nil)
    model.refresh(); model.completeSignIn(callback())
    try await eventually { model.activity == .idle }
    #expect(model.result?.uncertain == true)
    #expect(await stub.writes().count == 2)
}

@Test @MainActor func spotifyResumeWaitsForCancelledReadToDrainAndDropsOldResults() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    await stub.hold("GET /auth/spotify")
    model.refresh()
    try await gate(stub)
    model.suspend(); model.resume()
    await stub.setAuth(signed: true, pending: false)
    await stub.release()
    try await eventually { model.activity == .idle && model.signIn?.signedIn == true }
    #expect(await stub.maximumInFlight <= 2)
    #expect(await stub.writes().isEmpty)
}

@Test @MainActor func spotifyStatusFailuresStayIndependentAndDoNotExposeErrorEchoes() async throws {
    let stub = SpotifyStub()
    await stub.override("GET /auth/spotify", try jsonReply(["code": "NOT_IMPLEMENTED", "detail": callback()], status: 501))
    await stub.finish()
    let model = try await ready(stub)
    defer { model.stop() }
    #expect(model.signIn == nil && model.library?.last?.tracks == 100)
    #expect(model.canSync && !model.canBegin)
    #expect(model.signInProblem?.contains(testCode) == false)
    await stub.override("GET /auth/spotify", nil)
    await stub.override("GET /dj/sync", HTTPReply(data: Data("bad".utf8), status: 502))
    model.refresh()
    try await eventually { model.activity == .idle && model.libraryProblem != nil }
    #expect(model.signIn != nil && model.library == nil && !model.canSync)
}

@Test @MainActor func spotifyAlreadyRunningSyncPreflightDoesNotDispatchAgain() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    await stub.setRunning(true)
    model.syncLibrary()
    try await eventually { model.library?.running == true && model.activity == .idle }
    #expect(model.result?.title == "Library refresh already running")
    #expect(await stub.writes().isEmpty)
}

@Test @MainActor func spotifyFailureRetriesOnlyReadsAndStopsOnSuspension() async throws {
    let stub = SpotifyStub()
    await stub.override("GET /dj/sync", HTTPReply(data: Data(), status: 503))
    let model = try await ready(stub, poll: .milliseconds(30))
    defer { model.stop() }
    try await Task.sleep(for: .milliseconds(250))
    let count = await stub.count("/dj/sync")
    #expect(count >= 3 && count <= 5)
    model.suspend()
    try await eventually { model.activity == .idle }
    let stopped = await stub.requests.count
    try await Task.sleep(for: .milliseconds(70))
    #expect(await stub.requests.count == stopped)
    #expect(await stub.writes().isEmpty)
}

@Test @MainActor func spotifySlowBeginCannotBeOverlappedBySyncAndNeverOpensAnything() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    await stub.hold("POST /auth/spotify/begin")
    model.beginSignIn()
    try await gate(stub)
    model.syncLibrary(); model.beginSignIn(replacePending: true)
    model.suspend()
    await stub.release()
    try await eventually { model.activity == .idle }
    #expect(model.consent == nil)
    #expect(await stub.writes().count == 1)
    model.resume()
    try await eventually { model.activity == .idle && model.signIn?.pending == true }
    #expect(model.consent == nil && !model.canComplete)
}

@Test @MainActor func spotifyStopIsTerminalAndDiscardsConsent() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    model.beginSignIn()
    try await eventually { model.canComplete }
    model.stop(); model.resume(); model.syncLibrary(); model.completeSignIn(callback())
    #expect(!model.active && model.consent == nil)
    try await Task.sleep(for: .milliseconds(40))
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifyBrowserOpensOnlyAfterAnExplicitActionAndNeverReceivesACallback() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    var opened = [URL]()
    model.openConsent { opened.append($0); return true }
    #expect(opened.isEmpty)
    model.beginSignIn()
    try await eventually { model.canComplete }
    #expect(opened.isEmpty)
    model.openConsent { opened.append($0); return true }
    #expect(opened.count == 1 && opened[0].host == "accounts.spotify.com" && opened[0].path == "/authorize")
    #expect(!opened[0].absoluteString.contains(testCode))
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifyBrowserFailureRetainsTheLinkAndDoesNotRestartSignIn() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    model.openConsent { _ in false }
    #expect(model.browserProblem != nil && model.consent != nil)
    model.openConsent { _ in true }
    #expect(model.browserProblem == nil)
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifySuspendedSetupCannotLaunchTheBrowserAndCanResumeItsPendingFlow() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    model.suspend()
    var opens = 0
    model.openConsent { _ in opens += 1; return true }
    #expect(opens == 0)
    model.resume()
    try await eventually { model.canComplete }
    model.openConsent { _ in opens += 1; return true }
    #expect(opens == 1)
    #expect(await stub.writes().count == 1)
}

@Test @MainActor func spotifyBrowserActionDoesNotEraseAnUncertainLibraryRequest() async throws {
    let stub = SpotifyStub(), model = try await ready(stub)
    defer { model.stop() }
    model.beginSignIn()
    try await eventually { model.canComplete }
    await stub.loseWrites()
    model.syncLibrary()
    try await eventually { model.activity == .idle && model.result?.uncertain == true }
    model.openConsent { _ in true }
    #expect(model.result?.uncertain == true)
    #expect(await stub.writes().count == 2)
}
