import Foundation
import XCTest
@testable import LiveModel

@MainActor
final class LiveChecks: XCTestCase {
    static var failures = 0
    static let env = ProcessInfo.processInfo.environment
    static func check(_ row: Int, _ description: String, _ run: () async throws -> Void) async {
        do { try await run(); print("PASS row \(row): \(description)") }
        catch {
            failures += 1
            print("FAIL row \(row): \(description): \(error)")
            XCTFail("row \(row): \(error)")
        }
    }
    static func require(_ condition: Bool, _ message: String) throws {
        if !condition { throw CheckFailure(message: message) }
    }
    static func wait(_ message: String, seconds: Double = 12, _ condition: @escaping () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if condition() { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw CheckFailure(message: message)
    }
    static func control(_ command: String) async throws {
        let path = env["FSONOS_E2E_CONTROL"]!
        try command.write(toFile: path, atomically: true, encoding: .utf8)
        try await wait("daemon \(command) handshake") {
            (try? String(contentsOfFile: path, encoding: .utf8)) == (command == "stop" ? "stopped\n" : "started\n")
        }
    }
    static func fixture(_ name: String) throws -> Data {
        try Data(contentsOf: Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "Fixtures")!)
    }
    func testFailureMatrix() async throws { try await Self.runMatrix() }
    static func runMatrix() async throws {
        let url = URL(string: env["FSONOS_DAEMON_URL"]!)!
        let client = DaemonClient(baseURL: url)
        let store = LiveZoneStore(baseURL: url)
        await check(19, "bonjour finds an advertised daemon and resolves it to a URL") {
            let name = env["FSONOS_E2E_BONJOUR_NAME"]!
            let port = Int(env["FSONOS_E2E_BONJOUR_PORT"]!)!
            let browser = DaemonBrowser()
            browser.start()
            defer { browser.stop() }
            do {
                try await wait("bonjour discovery", seconds: 20) { browser.found.contains { $0.name == name } }
            } catch {
                throw CheckFailure(message: "bonjour discovery of '\(name)': saw \(browser.found.map(\.name))")
            }
            let hit = browser.found.first { $0.name == name }!
            try require(hit.url.port == port, "port was lost: \(hit.url)")
            // IPv4 when the registering host has one, else its .local name (this Mac publishes only IPv6 link-local).
            try require(hit.url.host.map { !$0.isEmpty } == true, "no host in \(hit.url)")
        }
        let stubURL = URL(string: env["FSONOS_E2E_STUB_URL"]!)!
        await check(20, "a refused live stream keeps the app connected and says why") {
            let stub = LiveZoneStore(baseURL: stubURL)
            stub.start()
            defer { stub.setActive(false) }
            try await wait("bootstrap over HTTP") { stub.connectionStatus == .live }
            try await wait("reason shown", seconds: 8) { stub.streamNote?.contains("may not use events") == true }
            // The stream is retried on a backoff. The status must hold steady through those retries.
            let until = Date().addingTimeInterval(4)
            while Date() < until {
                try require(stub.connectionStatus == .live, "refused stream flapped to \(stub.connectionStatus)")
                try await Task.sleep(for: .milliseconds(50))
            }
        }
        await check(21, "the copied log carries the context and the refusal, and stays bounded") {
            let report = AppLog.shared.report(header: ["Daemon: \(stubURL.absoluteString)"])
            try require(report.hasPrefix("FrankenSonos log"), "no title line:\n\(report.prefix(200))")
            try require(report.contains("Daemon: \(stubURL.absoluteString)"), "header line missing")
            try require(report.contains("events refused") && report.contains("POLICY_DENIED"), "refusal not logged:\n\(report.suffix(500))")
            for index in 0..<700 { AppLog.shared.add("test", "line \(index)") }
            let lines = AppLog.shared.report(header: []).split(separator: "\n")
            try require(lines.count <= AppLog.limit + 2, "log grew to \(lines.count) lines")
            try require(lines.last?.hasSuffix("line 699") == true, "newest line missing")
            try require(!lines.contains { $0.hasSuffix("line 0") }, "oldest line was not dropped")
        }
        await check(22, "a tap on play holds through the speaker's lag, and gives up if it never plays") {
            let house = LiveZoneStore(baseURL: stubURL)
            house.start()
            defer { house.setActive(false) }
            try await wait("stub house", seconds: 12) { house.zones.count == 2 && house.connectionStatus == .live }
            @MainActor func zoneID(_ room: String) -> UUID { house.zones.first { $0.roomNames == [room] }!.id }
            @MainActor func shown(_ id: UUID) -> Bool { house.zones.first { $0.id == id }?.isPlaying ?? false }
            // A real Sonos answers the first read after a resume with the old state, then "transitioning", then "playing".
            let lag = zoneID("Lag Room")
            try require(!shown(lag), "starts paused")
            house.togglePlayback(for: lag)
            try require(shown(lag), "the tap did not show playing at once")
            let until = Date().addingTimeInterval(5)
            while Date() < until {
                try require(shown(lag), "fell back to paused while the speaker caught up")
                try await Task.sleep(for: .milliseconds(20))
            }
            house.togglePlayback(for: lag)
            try require(!shown(lag), "the pause tap did not show paused at once")
            try await Task.sleep(for: .seconds(1.5))
            try require(!shown(lag), "a pause was undone by a late report")
            // A speaker that never starts must not be shown playing for ever.
            let stuck = zoneID("Stuck Room")
            house.togglePlayback(for: stuck)
            try require(shown(stuck), "the tap did not show playing")
            try await wait("gives up on a speaker that never plays", seconds: 12) { !shown(stuck) }
        }
        await check(23, "Spotify: sync to completion, then albums, search, album tracks and liked tracks") {
            let model = SpotifyModel(client: DaemonClient(baseURL: stubURL))
            await model.load()
            try require(model.phase == .empty, "signed in with an empty library should offer a sync, got \(model.phase)")
            await model.sync()
            try require(model.phase == .ready, "after the sync the library should be ready, got \(model.phase)")
            try require(model.albums.map(\.title) == ["Bach: Goldberg Variations, BWV 988", "Kind of Blue"], "albums: \(model.albums.map(\.title))")
            try require(model.albumsTotal == 2 && model.likedTotal == 2, "totals \(model.albumsTotal) \(model.likedTotal)")
            try require(model.albums[0].artUrl?.absoluteString == "https://cdn.example.invalid/bach.jpg", "art url lost")
            try require(model.albums[1].artUrl == nil, "an album without art must stay without")
            await model.search("miles")
            try require(model.albums.map(\.artist) == ["Miles Davis"], "search kept \(model.albums.map(\.artist))")
            await model.search("")
            try require(model.albums.count == 2, "clearing the search should restore the list")
            let tracks = await model.tracks(for: model.albums[0])
            try require(tracks.map(\.title) == ["Aria", "Variation 1", "Variation 2"], "tracks \(tracks.map(\.title))")
            try require(tracks[0].durationText == "3:03", "duration \(tracks[0].durationText)")
            try require(model.liked.map(\.title) == ["Aria", "Blue in Green"] && model.liked[1].artistLine == "Miles Davis, Bill Evans", "liked tracks")
        }
        await check(24, "Spotify: the real not-configured status explains itself, other states are told apart") {
            var status = try JSONDecoder().decode(SpotifyStatus.self, from: fixture("status-spotify-unconfigured.json"))
            try require(SpotifyModel.phase(status: status, unreachable: nil, albums: 0) == .notConfigured, "unconfigured")
            status.configured = true
            try require(SpotifyModel.phase(status: status, unreachable: nil, albums: 0) == .signedOut(reauthorize: false), "configured, not signed in")
            status.reauthorize = true
            try require(SpotifyModel.phase(status: status, unreachable: nil, albums: 0) == .signedOut(reauthorize: true), "revoked")
            status.signedIn = true
            status.reauthorize = false
            try require(SpotifyModel.phase(status: status, unreachable: nil, albums: 0) == .empty, "signed in, no library")
            try require(SpotifyModel.phase(status: status, unreachable: nil, albums: 3) == .ready, "signed in with a library")
            try require(SpotifyModel.phase(status: nil, unreachable: "timed out", albums: 0) == .unreachable("timed out"), "daemon unreachable")
            try require(SpotifyModel.phase(status: nil, unreachable: nil, albums: 0) == .loading, "first load")
        }
        await check(25, "Spotify: play and the DJ go to the selected room with what the app promised") {
            let house = LiveZoneStore(baseURL: stubURL)
            house.start()
            defer { house.setActive(false) }
            try await wait("stub house", seconds: 12) { house.zones.count == 2 && house.connectionStatus == .live }
            let id = house.zones.first { $0.roomNames == ["Lag Room"] }!.id
            house.selectedZoneID = id
            house.playSpotify(uri: "spotify:album:a1", title: "Bach: Goldberg Variations, BWV 988")
            try require(house.zones.first { $0.id == id }!.isPlaying, "playing a Spotify album should show playing at once")
            house.dj("start")
            house.dj("skip")
            house.dj("stop")
            try await wait("posts reach the daemon", seconds: 6) { true }
            var posts: [[String: Any]] = []
            for _ in 0..<60 {
                let (data, _) = try await URLSession.shared.data(from: stubURL.appendingPathComponent("_debug/posts"))
                posts = (try JSONSerialization.jsonObject(with: data) as? [[String: Any]]) ?? []
                if posts.count >= 4 { break }
                try await Task.sleep(for: .milliseconds(50))
            }
            let paths = posts.compactMap { $0["path"] as? String }
            try require(paths == ["/play", "/dj/start", "/dj/skip", "/dj/stop"], "posted \(paths)")
            let play = posts[0]["body"] as? [String: Any]
            try require(play?["source_uri"] as? String == "spotify:album:a1", "source_uri \(String(describing: play))")
            try require(play?["title"] as? String == "Bach: Goldberg Variations, BWV 988", "title")
            try require((play?["zone"] as? String)?.hasPrefix("Lag Room") == true, "zone \(String(describing: play?["zone"]))")
            try require((posts[1]["body"] as? [String: Any])?["zone"] as? String == play?["zone"] as? String, "the DJ must act on the same room")
        }
        await check(26, "Spotify sign-in: PKCE vector, authorize URL, and every callback shape") {
            // RFC 7636 appendix B.
            try require(SpotifyAuth.challenge(for: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk") == "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "challenge does not match the RFC vector")
            let verifier = SpotifyAuth.makeVerifier()
            try require((43...128).contains(verifier.count) && verifier.allSatisfy { $0.isLetter || $0.isNumber || "-._~".contains($0) }, "verifier shape: \(verifier)")
            try require(SpotifyAuth.makeVerifier() != verifier, "verifiers must differ")
            let url = SpotifyAuth.authorizeURL(clientID: "cid", redirectURI: "frankensonos://spotify-callback", challenge: "CH", state: "ST")
            let items = Dictionary(uniqueKeysWithValues: URLComponents(url: url, resolvingAgainstBaseURL: false)!.queryItems!.map { ($0.name, $0.value ?? "") })
            try require(url.host == "accounts.spotify.com" && url.path == "/authorize", "authorize endpoint \(url)")
            try require(items == ["response_type": "code", "client_id": "cid", "redirect_uri": "frankensonos://spotify-callback", "code_challenge_method": "S256", "code_challenge": "CH", "state": "ST", "scope": "user-library-read"], "authorize query \(items)")
            func parse(_ query: String) -> Result<String, SpotifyAuthError> {
                Result { try SpotifyAuth.code(from: URL(string: "frankensonos://spotify-callback?\(query)")!, expectedState: "ST") }.mapError { $0 as! SpotifyAuthError }
            }
            try require(parse("code=abc&state=ST") == .success("abc"), "good callback")
            try require(parse("code=abc&state=OTHER") == .failure(.stateMismatch), "state mismatch must be refused")
            try require(parse("code=abc") == .failure(.stateMismatch), "a callback without state must be refused")
            try require(parse("error=access_denied&state=ST") == .failure(.denied("access_denied")), "denied")
            try require(parse("state=ST") == .failure(.missingCode), "no code")
            try require(parse("code=&state=ST") == .failure(.missingCode), "empty code")
        }
        await check(27, "Spotify sign-in: the app sends the daemon exactly the exchange it promised, then syncs") {
            func post(_ path: String, _ body: [String: Any] = [:]) async throws {
                var request = URLRequest(url: stubURL.appendingPathComponent(path))
                request.httpMethod = "POST"
                request.httpBody = try JSONSerialization.data(withJSONObject: body)
                _ = try await URLSession.shared.data(for: request)
            }
            func exchanges() async throws -> [[String: Any]] {
                let (data, _) = try await URLSession.shared.data(from: stubURL.appendingPathComponent("_debug/posts"))
                return ((try JSONSerialization.jsonObject(with: data) as? [[String: Any]]) ?? []).filter { $0["path"] as? String == "/auth/spotify/exchange" }
            }
            func state(of url: URL) -> String {
                URLComponents(url: url, resolvingAgainstBaseURL: false)!.queryItems!.first { $0.name == "state" }!.value!
            }
            try await post("_debug/signout")
            let model = SpotifyModel(client: DaemonClient(baseURL: stubURL))
            await model.load()
            try require(model.phase == .signedOut(reauthorize: false), "stub should start signed out, got \(model.phase)")
            try require(model.status?.clientID == "stub-client" && model.status?.appRedirectURI == "frankensonos://spotify-callback", "status lost the client id or redirect")
            // S3: Spotify refuses. S4: wrong state. S2: the person closes the sheet. None of them reaches the daemon.
            await model.signIn { url in URL(string: "frankensonos://spotify-callback?error=access_denied&state=\(state(of: url))")! }
            try require(model.error?.contains("access_denied") == true, "denial not shown: \(String(describing: model.error))")
            await model.signIn { _ in URL(string: "frankensonos://spotify-callback?code=x&state=forged")! }
            try require(model.error?.isEmpty == false, "a forged state must be reported")
            await model.signIn { _ in throw SpotifySignInCancelled() }
            try require(model.error == nil, "closing the sheet is not an error, got \(String(describing: model.error))")
            try require(try await exchanges().isEmpty, "nothing may reach the daemon before a valid callback")
            var captured: URL?
            await model.signIn { url in
                captured = url
                return URL(string: "frankensonos://spotify-callback?code=the-code&state=\(state(of: url))")!
            }
            let sent = try await exchanges()
            try require(sent.count == 1, "expected one exchange, got \(sent.count)")
            let body = sent[0]["body"] as? [String: String]
            let challenge = URLComponents(url: captured!, resolvingAgainstBaseURL: false)!.queryItems!.first { $0.name == "code_challenge" }!.value!
            try require(body?["code"] == "the-code" && body?["redirect_uri"] == "frankensonos://spotify-callback", "exchange body \(String(describing: body))")
            try require(body?["code_verifier"].map(SpotifyAuth.challenge(for:)) == challenge, "the verifier does not match the challenge sent to Spotify")
            try require(model.error == nil && model.phase == .ready, "after sign-in the library should be read, got \(model.phase) \(String(describing: model.error))")
            // S1: with no daemon to ask, sign-in says why instead of opening a sheet.
            let bare = SpotifyModel(client: DaemonClient(baseURL: URL(string: "http://127.0.0.1:9")!))
            var opened = false
            await bare.signIn { _ in opened = true; throw SpotifySignInCancelled() }
            try require(bare.error != nil && !opened, "sign-in against an unreachable daemon must say why and open nothing")
        }
        await check(28, "album art: the daemon's relative art URL resolves against its address, an https URL stays") {
            let house = LiveZoneStore(baseURL: stubURL)
            house.start()
            defer { house.setActive(false) }
            try await wait("stub house", seconds: 12) { house.zones.count == 2 && house.connectionStatus == .live }
            try await wait("art urls", seconds: 12) {
                let lag = house.zones.first { $0.roomNames == ["Lag Room"] }?.track.artURL?.absoluteString
                let stuck = house.zones.first { $0.roomNames == ["Stuck Room"] }?.track.artURL?.absoluteString
                return lag == "\(stubURL.absoluteString)/speaker-art?player=RINCON_TEST&u=%2Fgetaa%3Fs%3D1%26u%3Dx" && stuck == "https://cdn.example.invalid/x.jpg"
            }
        }
        await check(29, "pasted Spotify links: every shape becomes a canonical URI, every bad one says why") {
            let id = "4uLU6hMCjMI75M1A2tKUQC"
            func uri(_ text: String) -> String? { if case .success(let link) = SpotifyLink.parse(text) { return link.uri } else { return nil } }
            func failure(_ text: String) -> SpotifyLinkError? { if case .failure(let error) = SpotifyLink.parse(text) { return error } else { return nil } }
            try require(uri("spotify:track:\(id)") == "spotify:track:\(id)", "C1")
            try require(uri("https://open.spotify.com/track/\(id)?si=abc123#x") == "spotify:track:\(id)", "C2 query and fragment must go")
            try require(uri("https://open.spotify.com/intl-de/album/\(id)") == "spotify:album:\(id)", "C3 locale segment")
            try require(uri("  \n Listen to this https://open.spotify.com/playlist/\(id)?si=z  \n") == "spotify:playlist:\(id)", "C8 text around the link")
            try require(uri("http://open.spotify.com/artist/\(id)") == "spotify:artist:\(id)", "http is accepted")
            try require(failure("https://open.spotify.com/user/\(id)") == .unsupportedKind, "C4 kind")
            try require(failure("spotify:radio:\(id)") == .unsupportedKind, "C4 uri kind")
            try require(failure("https://spotify.link/AbCdEf") == .shortLink, "C5")
            try require(failure("https://open.spotify.com/track/abc") == .cutOff, "C6 short id")
            try require(failure("spotify:track:\(id)xx") == .cutOff, "C6 long id")
            try require(failure("https://example.com/track/\(id)") == .notSpotify, "C7 other site")
            try require(failure("") == .notSpotify && failure("hello") == .notSpotify, "C7 plain text")
            try require(failure("https://evil.example/?u=https://open.spotify.com/track/\(id)") == nil, "a Spotify link inside another site's URL is still a Spotify link in the text")
            try require(SpotifyLink.parse("spotify:track:\(id)") == .success(SpotifyLink(kind: "track", id: id)), "kind and id")
            try require(SpotifyLinkError.shortLink.message.contains("open.spotify.com"), "the short-link message must say what to do")
        }
        await check(30, "Siri and Shortcuts: pause and resume reach the right room and say what they did") {
            let remote = RoomRemote(client: DaemonClient(baseURL: stubURL))
            func commands() async throws -> [[String: Any]] {
                let (data, _) = try await URLSession.shared.data(from: stubURL.appendingPathComponent("_debug/commands"))
                return (try JSONSerialization.jsonObject(with: data) as? [[String: Any]]) ?? []
            }
            let before = try await commands().count
            let paused = try await remote.pause(room: "  lag room ")
            try require(paused == "Paused Lag Room.", "I9 pause summary: \(paused)")
            let resumed = try await remote.resume(room: "Lag Room")
            try require(resumed == "Resumed Lag Room.", "I9 resume summary: \(resumed)")
            var sent = Array(try await commands().dropFirst(before))
            try require(sent.map { $0["path"] as? String } == ["/pause", "/resume"], "paths \(sent)")
            try require(sent.allSatisfy { ($0["body"] as? [String: String]) == ["zone": "Lag Room"] }, "I2/I9 the daemon must get the room's real name: \(sent)")
            do {
                _ = try await remote.pause(room: "Garage")
                throw CheckFailure(message: "I1 an unknown room was accepted")
            } catch let error as RoomRemoteError {
                try require(error.message == "I don't know a room called Garage. Rooms: Lag Room, Stuck Room.", "I1 message: \(error.message)")
            }
            sent = Array(try await commands().dropFirst(before))
            try require(sent.count == 2, "I1 nothing may be sent for an unknown room")
            do {
                _ = try await RoomRemote(client: DaemonClient(baseURL: URL(string: "http://127.0.0.1:9")!)).pause(room: "Lag Room")
                throw CheckFailure(message: "I6 an unreachable daemon was accepted")
            } catch let error as RoomRemoteError {
                try require(error.message == "I can't reach the daemon.", "I6 message: \(error.message)")
            }
            do {
                _ = try await RoomRemote(client: nil).pause(room: "Lag Room")
                throw CheckFailure(message: "I7 no daemon was accepted")
            } catch let error as RoomRemoteError {
                try require(error.message == "Open FrankenSonos and choose a daemon first.", "I7 message: \(error.message)")
            }
        }
        await check(31, "Siri and Shortcuts: favorites match by name, and an unclear name sends nothing") {
            let remote = RoomRemote(client: DaemonClient(baseURL: stubURL))
            func commands() async throws -> [[String: Any]] {
                let (data, _) = try await URLSession.shared.data(from: stubURL.appendingPathComponent("_debug/commands"))
                return (try JSONSerialization.jsonObject(with: data) as? [[String: Any]]) ?? []
            }
            let before = try await commands().count
            let said = try await remote.playFavorite("jazz mix", room: "Lag Room")
            try require(said == "Playing Jazz Mix in Lag Room.", "I10 summary: \(said)")
            let unique = try await remote.playFavorite("morning", room: "Lag Room")
            try require(unique == "Playing Morning News in Lag Room.", "I5 unique substring: \(unique)")
            for (name, expected) in [("Podcasts", "No favorite called Podcasts in Lag Room."), ("jazz", "More than one favorite matches jazz: Jazz Classics, Jazz Mix.")] {
                do {
                    _ = try await remote.playFavorite(name, room: "Lag Room")
                    throw CheckFailure(message: "an unclear favorite name '\(name)' was accepted")
                } catch let error as RoomRemoteError {
                    try require(error.message == expected, "message for \(name): \(error.message)")
                }
            }
            let sent = Array(try await commands().dropFirst(before))
            try require(sent.count == 2 && sent.allSatisfy { $0["path"] as? String == "/play/favorite" }, "only the two clear requests may be sent: \(sent)")
            try require((sent[0]["body"] as? [String: String]) == ["zone": "Lag Room", "favorite": "FV:2/1"], "I10 body \(sent[0])")
        }
        await check(1, "unreachable launch stays empty/offline, then bootstraps") {
            try await control("stop")
            store.start()
            try await wait("offline launch") { store.connectionStatus == .offline }
            try require(store.zones.isEmpty, "offline launch showed sample rooms")
            try await control("start")
            try await wait("live bootstrap", seconds: 25) { store.connectionStatus == .live && store.zones.count == 2 }
            try require(Set(store.rooms) == ["Bedroom", "Living Room"], "incorrect inventory")
        }
        guard let id = store.zones.first(where: { $0.roomNames.first == "Living Room" })?.id else {
            XCTFail("missing bootstrap zone"); return
        }
        await check(10, "favorite playback and external track event refetch metadata") {
            try await wait("favorites") { !store.tracks.isEmpty }
            store.selectTrack(store.tracks.first(where: { $0.title == "Nocturne in E-flat" })!, in: id)
            try await wait("favorite state") { store.zones.first(where: { $0.id == id })?.track.title == "Nocturne in E-flat" }
            try await waitForTrack("Nocturne in E-flat", client: client)
            try await client.command("play/favorite", body: ["zone": "Living Room", "favorite": "FV:2/1"])
            try await wait("SSE track metadata") { store.zones.first(where: { $0.id == id })?.track.title == "Aria" }
        }
        await check(8, "volume drag is immediate, debounced and last write wins") {
            for value in [0.31, 0.32, 0.33, 0.34, 0.35] { store.setVolume(value, for: id) }
            try require(store.zones.first(where: { $0.id == id })?.volume == 0.35, "volume was not optimistic")
            try await Task.sleep(for: .seconds(1))
            let state = try await client.state(room: "Living Room")
            try require(state.volume == 35, "last volume did not reach speaker: \(String(describing: state.volume))")
            try require(store.roomVolumes["Living Room"] == 0.35, "echo overwrote slider")
        }
        await check(6, "bad room command reverts optimistic volume with daemon detail") {
            let before = store.roomVolumes["Missing Room"]
            store.setRoomVolume(0.61, room: "Missing Room")
            try require(store.roomVolumes["Missing Room"] == 0.61, "bad-room volume was not optimistic")
            try await wait("4xx toast") { store.commandError != nil }
            try require(store.roomVolumes["Missing Room"] == before, "4xx left optimistic volume behind")
            try require(store.commandError!.contains("Missing Room"), "daemon detail was lost")
        }
        await check(9, "group/ungroup topology stays idempotent") {
            store.setGroupedRooms(["Living Room", "Bedroom"], basedOn: id)
            try await wait("group") { store.zones.count == 1 && store.zones[0].roomNames.count == 2 }
            try await Task.sleep(for: .seconds(1))
            let grouped = try await client.zones()
            try require(grouped.count == 1 && Set(grouped[0].members) == Set(store.zones[0].roomNames), "group differs from daemon")
            store.setGroupedRooms(["Living Room"], basedOn: id)
            try await wait("ungroup") { store.zones.count == 2 }
            try await Task.sleep(for: .seconds(1))
            try require(try await client.zones().count == 2, "ungroup differs from daemon")
            try require(store.zones.contains(where: { $0.id == id }), "coordinator ID changed")
        }
        await check(2, "SSE disconnect reconnects and observes later external commands") {
            try await wait("event cursor") { store.lastEventID != nil }
            try await control("stop")
            try await wait("reconnecting") { store.connectionStatus == .reconnecting }
            try await control("start")
            try await wait("reconnected", seconds: 25) { store.connectionStatus == .live }
            try await client.command("volume", body: ["zone": "Living Room", "volume": 26])
            try await wait("event after reconnect", seconds: 20) { store.roomVolumes["Living Room"] == 0.26 }
        }
        await check(7, "unreachable command reverts and reports failure") {
            try await control("stop")
            let before = store.roomVolumes["Living Room"]
            store.commandError = nil
            store.setRoomVolume(0.58, room: "Living Room")
            try await wait("network toast") { store.commandError != nil }
            try require(store.roomVolumes["Living Room"] == before, "network failure left optimistic volume")
            try require(store.commandError!.contains("Didn't go through"), "network failure message lost")
            try await control("start")
            try await wait("live again", seconds: 25) { store.connectionStatus == .live }
        }
        await check(17, "foreground fully refreshes after background cancellation") {
            store.setActive(false)
            try await client.command("volume", body: ["zone": "Living Room", "volume": 29])
            store.setActive(true)
            do {
                try await wait("foreground volume") { store.roomVolumes["Living Room"] == 0.29 && store.connectionStatus == .live }
            } catch {
                let seen = "volume=\(String(describing: store.roomVolumes["Living Room"])) status=\(store.connectionStatus)"
                throw CheckFailure(message: "foreground volume: \(seen)")
            }
        }
        await check(11, "captured streams and absent tracks have no scrubber duration") {
            let decoder = JSONDecoder()
            let now = try decoder.decode(ZoneStateDTO.self, from: fixture("state-now.json"))
            let stopped = try decoder.decode(ZoneStateDTO.self, from: fixture("state-stopped.json"))
            try require(now.track?.duration_secs == 0 && stopped.track == nil, "optional track/duration contract")
            let missing = try decoder.decode(TrackDTO.self, from: Data("{\"uri\":\"stream:placeholder\"}".utf8))
            try require(missing.duration_secs == nil && missing.title == nil, "missing metadata failed")
            try require(store.zones.first(where: { $0.id == id })!.track.duration == 0, "stream got a fictional duration")
        }
        await check(12, "playing position ticks locally and pauses") {
            let track = Track.live(title: "Placeholder", artist: "", album: "", key: "placeholder", duration: 120)
            try require(track.duration == 120, "duration mapping")
            let before = store.elapsed
            store.tick(now: Date().addingTimeInterval(3))
            try require(store.elapsed >= before, "position moved backward")
            store.togglePlayback(for: id)
            try await wait("paused") { !store.zones.first(where: { $0.id == id })!.isPlaying }
            try await waitForTransport("paused", client: client)
            store.togglePlayback(for: id)
            try await wait("resumed") { store.zones.first(where: { $0.id == id })!.isPlaying }
        }
        await check(13, "spaces, accents and plus use one encoded path segment") {
            let special = client.stateURL(room: "Pièce + Room")
            try require(special.absoluteString.contains("Pi%C3%A8ce%20%2B%20Room"), "wrong path: \(special)")
            try require(try await client.state(room: "Living Room").zone.coordinator_room == "Living Room", "space route failed")
        }
        await check(15, "fixture decoding tolerates unknown fields and SSE kinds") {
            try checkJSONFixtures()
            try checkSSEFixture()
            try await checkUnknownEvent(store)
        }
        store.setActive(false)
        print("Live checks: \(failures) failed")
    }

    static func checkJSONFixtures() throws {
        let decoder = JSONDecoder()
        let zonesData = try fixture("zones.json")
        let roomsData = try fixture("rooms.json")
        let favoritesData = try fixture("favorites.json")
        let zones = try decoder.decode([ZoneDTO].self, from: zonesData)
        let rooms = try decoder.decode([RoomDTO].self, from: roomsData)
        let favorites = try decoder.decode([FavoriteDTO].self, from: favoritesData)
        try require(zones.count == 2, "zone fixture count")
        try require(rooms.count == 2, "room fixture count")
        try require(favorites.count == 5, "favorite fixture count")
        let mutated = Data("{\"uri\":\"placeholder\",\"future\":{\"nested\":true}}".utf8)
        _ = try decoder.decode(TrackDTO.self, from: mutated)
    }
    static func waitForTrack(_ title: String, client: DaemonClient) async throws {
        let deadline = Date().addingTimeInterval(12)
        while Date() < deadline {
            let state = try await client.state(room: "Living Room")
            if state.track?.title == title { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw CheckFailure(message: "favorite command did not reach daemon: \(title)")
    }
    /// The store updates optimistically, so the daemon sees the command a moment later.
    static func waitForTransport(_ expected: String, client: DaemonClient) async throws {
        let deadline = Date().addingTimeInterval(12)
        while Date() < deadline {
            if try await client.state(room: "Living Room").transport_state == expected { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        throw CheckFailure(message: "pause did not reach speaker")
    }
    static func checkSSEFixture() throws {
        let bytes = try fixture("events.sse")
        let lines = String(decoding: bytes, as: UTF8.self).components(separatedBy: "\n")
        var parser = SSEParser()
        var frames: [DaemonEvent] = []
        for line in lines {
            if let frame = parser.consume(line) { frames.append(frame) }
        }
        try require(frames.first?.id == "9", "first SSE cursor")
        try require(frames.last?.id == "20", "last SSE cursor")
    }
    static func checkUnknownEvent(_ store: LiveZoneStore) async throws {
        let count = store.zones.count
        let event = DaemonEvent(id: "future", kind: "future.kind", data: "{}")
        await store.apply(event)
        try require(store.zones.count == count, "unknown event changed inventory")
    }
}

struct CheckFailure: Error, CustomStringConvertible {
    let message: String
    var description: String { message }
}
