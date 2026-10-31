import Foundation

public enum LibraryCategory: String, Codable, CaseIterable, Sendable {
    case tracks, albums, playlists
    public var title: String {
        switch self { case .tracks: "Tracks"; case .albums: "Albums"; case .playlists: "Playlists" }
    }
    var itemKind: String {
        switch self { case .tracks: "track"; case .albums: "album"; case .playlists: "playlist" }
    }
}

public enum LibraryCatalogError: Error, LocalizedError, Equatable, Sendable {
    case invalidQuery, invalidPage, changedRevision, selectionExpired
    public var errorDescription: String? {
        switch self {
        case .invalidQuery: "Use a filter of at most 512 UTF-8 bytes without control characters."
        case .invalidPage: "The cached library page is incomplete or inconsistent. Refresh the listing."
        case .changedRevision: "Your library changed while browsing. Refresh from the beginning; pages from different versions were not combined."
        case .selectionExpired: "This library selection changed. Choose it again from the refreshed listing."
        }
    }
}

/// The public cached-browse contract, independent of speakers and Spotify I/O.
public struct LibraryBrowseRequest: Equatable, Sendable {
    public static let pageSize: UInt32 = 50
    public let category: LibraryCategory
    public let query: String
    public let offset: UInt32
    public let albumUri: String?

    public init(category: LibraryCategory = .albums, query: String = "", offset: UInt32 = 0,
                albumUri: String? = nil) throws {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.utf8.count <= 512,
              !trimmed.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            throw LibraryCatalogError.invalidQuery
        }
        guard albumUri == nil || (category == .tracks && Self.source(albumUri!, kind: "album")) else {
            throw LibraryCatalogError.invalidPage
        }
        self.category = category; self.query = trimmed; self.offset = offset; self.albumUri = albumUri
    }

    public var queryItems: [URLQueryItem] {
        var items = [URLQueryItem(name: "kind", value: category.rawValue),
                     URLQueryItem(name: "offset", value: String(offset)),
                     URLQueryItem(name: "limit", value: String(Self.pageSize))]
        if !query.isEmpty { items.append(URLQueryItem(name: "q", value: query)) }
        if let albumUri { items.append(URLQueryItem(name: "album_uri", value: albumUri)) }
        return items
    }

    static func source(_ uri: String, kind: String) -> Bool {
        let pieces = uri.split(separator: ":", omittingEmptySubsequences: false)
        return pieces.count == 3 && pieces[0] == "spotify" && pieces[1] == kind
            && !pieces[2].isEmpty && pieces[2].utf8.count <= 256
            && pieces[2].allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber) }
    }
}

public struct LibraryCatalogItem: Decodable, Equatable, Identifiable, Sendable {
    public let sourceUri: String
    public let title: String
    public let kind: String
    public let subtitle: String?
    public let explicit: Bool
    public let albumUri: String?
    public let cachedTracks: UInt32?
    public let discNumber: UInt32?
    public let trackNumber: UInt32?
    private let explicitKnownValue: Bool?
    fileprivate var occurrenceIdentity: String? = nil
    public var explicitKnown: Bool { explicitKnownValue ?? true }
    public var id: String { occurrenceIdentity ?? sourceUri }

    private enum CodingKeys: String, CodingKey {
        case sourceUri, title, kind, subtitle, explicit, albumUri, cachedTracks, discNumber, trackNumber
        case explicitKnownValue = "explicitKnown"
    }
}

/// A complete observation is the daemon's last fully fetched playable album,
/// not a claim that the source is still available or that all metadata is known.
public struct LibraryAlbumObservation: Decodable, Equatable, Sendable {
    public let coverage: String
    public let fetchedAt: Int64?
    public let cachedTotal: UInt32

    public var description: String {
        coverage == "complete"
            ? "Complete cached album: \(cachedTotal) playable track positions"
            : "Saved-library subset: \(cachedTotal) cached tracks"
    }
}

public struct LibraryCatalogPage: Decodable, Equatable, Sendable {
    public private(set) var items: [LibraryCatalogItem]
    public let total: UInt32
    public let offset: UInt32
    public let limit: UInt32
    public let nextOffset: UInt32?
    public let kind: LibraryCategory
    public let albumUri: String?
    public let revision: String
    public let coverage: String
    public let albumObservation: LibraryAlbumObservation?

    /// Never join pages across revisions or guess identities from titles.
    public func validated(for request: LibraryBrowseRequest) throws -> Self {
        let count = min(LibraryBrowseRequest.pageSize, total > offset ? total - offset : 0)
        let end = UInt64(offset) + UInt64(items.count)
        let expectedNext = end < UInt64(total) ? UInt32(exactly: end) : nil
        guard kind == request.category, offset == request.offset,
              limit == LibraryBrowseRequest.pageSize, albumUri == request.albumUri,
              coverage == "cached", !revision.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              revision.utf8.count <= 256, items.count == Int(count), nextOffset == expectedNext else {
            throw LibraryCatalogError.invalidPage
        }
        let complete = albumObservation?.coverage == "complete"
        if let observation = albumObservation {
            guard request.albumUri != nil, observation.cachedTotal >= total,
                  observation.coverage == "complete" || observation.coverage == "library_subset",
                  complete ? observation.fetchedAt.map({ (0...8_640_000_000_000).contains($0) }) == true : observation.fetchedAt == nil else {
                throw LibraryCatalogError.invalidPage
            }
        }
        var result = self
        var identities = Set<String>()
        var previous: (UInt32, UInt32)?
        result.items = try items.map { original in
            var item = original
            guard item.kind == kind.itemKind, LibraryBrowseRequest.source(item.sourceUri, kind: item.kind),
                  request.albumUri == nil || item.albumUri == request.albumUri,
                  kind != .albums || (item.cachedTracks ?? 0) > 0,
                  item.explicitKnown || (complete && !item.explicit) else {
                throw LibraryCatalogError.invalidPage
            }
            item.occurrenceIdentity = nil
            if complete {
                guard let album = request.albumUri, let disc = item.discNumber, disc > 0,
                      let track = item.trackNumber, track > 0 else { throw LibraryCatalogError.invalidPage }
                if let previous {
                    guard disc > previous.0 || (disc == previous.0 && track > previous.1) else {
                        throw LibraryCatalogError.invalidPage
                    }
                }
                previous = (disc, track)
                item.occurrenceIdentity = "\(album)#disc:\(disc):track:\(track)"
            }
            guard identities.insert(item.id).inserted else { throw LibraryCatalogError.invalidPage }
            return item
        }
        return result
    }
}

/// A UI selection is tied to a particular accepted page, not a mutable row index.
public struct LibraryCatalogSelection: Equatable, Sendable {
    public let item: LibraryCatalogItem
    fileprivate let pageID: UUID
}

/// Browsing never sends a playback command. These intents enter the remote's
/// existing household-qualified command review, just like other music actions.
public enum LibraryCatalogAction: Sendable, CaseIterable {
    case playNow, queueNext, queueLast

    fileprivate func command(source: String) -> NativeCommand {
        switch self {
        case .playNow: .playSource(source)
        case .queueNext: .queueAdd(source, next: true)
        case .queueLast: .queueAdd(source, next: false)
        }
    }
}

extension DaemonClient {
    public func libraryCatalog(_ request: LibraryBrowseRequest) async throws -> LibraryCatalogPage {
        try await get(LibraryCatalogPage.self, ["library", "browse"], query: request.queryItems)
            .validated(for: request)
    }
}

/// A bounded, read-only browser. Playback stays with CommandReviewSession.
/// At most one read runs; an obsolete/cancelled reader must drain before the
/// newest navigation intent starts. A transport ignoring cancellation cannot
/// repaint, launch overlapping reads, or revive an earlier selection.
@MainActor public final class LibraryCatalogModel {
    public typealias Reader = @Sendable (LibraryBrowseRequest) async throws -> LibraryCatalogPage
    public var onChange: (@MainActor () -> Void)?
    public let playlist: PlaylistContentsModel
    public private(set) var request: LibraryBrowseRequest { willSet { onChange?() } }
    public private(set) var page: LibraryCatalogPage? { willSet { onChange?() } }
    public private(set) var observedAt: Date? { willSet { onChange?() } }
    public private(set) var isFresh = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public private(set) var restartRequired = false { willSet { onChange?() } }
    public private(set) var album: LibraryCatalogItem? { willSet { onChange?() } }

    private struct ReadIntent {
        let request: LibraryBrowseRequest
        let revision: String?
        let epoch: UInt64
        let delay: Duration
    }
    private let read: Reader
    private let now: @Sendable () -> Date
    private let searchDelay: Duration
    private var epoch: UInt64 = 0
    private var pageID = UUID()
    private var paginationRevision: String?
    private var filterInvalid = false
    private var stopped = false
    private var task: Task<Void, Never>?
    private var pending: ReadIntent?
    private var albumReturn: (LibraryBrowseRequest, String)?

    public init(searchDelay: Duration = .milliseconds(350), now: @escaping @Sendable () -> Date = { Date() },
                playlistRead: @escaping PlaylistContentsModel.Reader = { _ in
                    throw ContractError.invalid("A playlist reader was not configured.")
                }, read: @escaping Reader) {
        self.read = read; self.now = now; self.searchDelay = searchDelay
        playlist = PlaylistContentsModel(read: playlistRead)
        // Known constants, not input from a server or user.
        request = try! LibraryBrowseRequest()
        playlist.onChange = { [weak self] in self?.onChange?() }
    }

    public convenience init(client: DaemonClient, searchDelay: Duration = .milliseconds(350),
                            now: @escaping @Sendable () -> Date = { Date() }) {
        self.init(searchDelay: searchDelay, now: now,
                  playlistRead: { try await client.playlistContents($0) },
                  read: { try await client.libraryCatalog($0) })
    }

    public var canChoose: Bool { active && !stopped && !playlist.isOpen && isFresh && !loading && page != nil && !restartRequired }
    public var canRefresh: Bool {
        active && !stopped && (playlist.isOpen ? playlist.canRefresh : !filterInvalid && !loading)
    }
    public var canGoBack: Bool { canChoose && request.offset > 0 }
    public var canGoForward: Bool { canChoose && page?.nextOffset != nil }
    public var canChooseAlbum: Bool {
        canChoose && album?.kind == "album" && album?.sourceUri == request.albumUri
            && page?.albumUri == request.albumUri && request.category == .tracks
    }

    public func resume() {
        guard !active && !stopped else { return }
        active = true
        if playlist.isOpen { playlist.resume(); return }
        guard !restartRequired && !filterInvalid else { return }
        enqueue(request, revision: page?.revision ?? paginationRevision)
    }
    public func suspend() {
        playlist.suspend()
        active = false; epoch &+= 1; pending = nil; task?.cancel()
        isFresh = false; loading = false; pageID = UUID()
    }
    public func stop() { suspend(); playlist.stop(); stopped = true }

    /// Explicit refresh restarts at zero; never automatically splice a changed listing.
    public func refresh() {
        guard active && !stopped else { return }
        if playlist.isOpen { playlist.refresh(); return }
        guard !filterInvalid else { return }
        guard let first = try? LibraryBrowseRequest(category: request.category, query: request.query,
                                                   albumUri: request.albumUri) else { return }
        enqueue(first, revision: nil)
    }

    public func browse(_ category: LibraryCategory) {
        guard active && !stopped else { return }
        playlist.close()
        album = nil; albumReturn = nil; filterInvalid = false
        guard let next = try? LibraryBrowseRequest(category: category, query: request.query) else { return }
        enqueue(next, revision: nil)
    }

    public func filter(_ text: String) {
        guard active && !stopped && !playlist.isOpen else { return }
        do {
            let next = try LibraryBrowseRequest(category: request.category, query: text, albumUri: request.albumUri)
            let recovering = filterInvalid
            filterInvalid = false
            guard next.query != request.query || recovering else { return }
            enqueue(next, revision: nil, delay: searchDelay)
        } catch {
            filterInvalid = true
            epoch &+= 1; pending = nil; task?.cancel(); pageID = UUID()
            isFresh = false; loading = false; problem = error.localizedDescription
        }
    }

    public func nextPage() {
        guard canGoForward, let page, let offset = page.nextOffset,
              let next = try? LibraryBrowseRequest(category: request.category, query: request.query,
                                                  offset: offset, albumUri: request.albumUri) else { return }
        enqueue(next, revision: page.revision)
    }
    public func previousPage() {
        guard canGoBack, let page,
              let previous = try? LibraryBrowseRequest(category: request.category, query: request.query,
                  offset: request.offset >= LibraryBrowseRequest.pageSize ? request.offset - LibraryBrowseRequest.pageSize : 0,
                  albumUri: request.albumUri) else { return }
        enqueue(previous, revision: page.revision)
    }

    public func selection(for id: String) -> LibraryCatalogSelection? {
        guard canChoose, let item = page?.items.first(where: { $0.id == id }) else { return nil }
        return LibraryCatalogSelection(item: item, pageID: pageID)
    }
    public func resolve(_ selection: LibraryCatalogSelection) -> LibraryCatalogItem? {
        guard canChoose, selection.pageID == pageID,
              page?.items.contains(selection.item) == true else { return nil }
        return selection.item
    }

    public func command(for selection: LibraryCatalogSelection, action: LibraryCatalogAction) throws -> NativeCommand {
        guard let item = resolve(selection) else { throw LibraryCatalogError.selectionExpired }
        return action.command(source: item.sourceUri)
    }

    public func albumSelection() -> LibraryCatalogSelection? {
        guard canChooseAlbum, let album else { return nil }
        return LibraryCatalogSelection(item: album, pageID: pageID)
    }

    /// The album source is authoritative only while its current track page is
    /// fresh. Capture this selection when showing its menu so an old album
    /// action cannot silently target a newly opened album or refreshed page.
    public func command(forAlbum selection: LibraryCatalogSelection, action: LibraryCatalogAction) throws -> NativeCommand {
        guard canChooseAlbum, selection.pageID == pageID, selection.item == album else {
            throw LibraryCatalogError.selectionExpired
        }
        return action.command(source: selection.item.sourceUri)
    }

    /// Resolve the exact catalog page before opening; never derive a playlist URI from its label.
    public func openPlaylist(_ selection: LibraryCatalogSelection) throws {
        guard let item = resolve(selection), item.kind == "playlist" else {
            throw LibraryCatalogError.selectionExpired
        }
        try playlist.open(playlistUri: item.sourceUri, name: item.title)
    }
    public func backToPlaylists() {
        guard active && !stopped && playlist.isOpen else { return }
        playlist.close()
        // Keep the original name filter and page, but require the listing's revision.
        enqueue(request, revision: page?.revision ?? paginationRevision)
    }

    public func openAlbum(_ selection: LibraryCatalogSelection) {
        guard let item = resolve(selection), item.kind == "album", let page,
              let tracks = try? LibraryBrowseRequest(category: .tracks, albumUri: item.sourceUri) else { return }
        albumReturn = (request, page.revision); album = item
        // The album-title filter must not hide its differently named movements.
        enqueue(tracks, revision: nil)
    }
    public func backToAlbums() {
        guard active && !stopped, let (previous, revision) = albumReturn else { return }
        album = nil; albumReturn = nil
        enqueue(previous, revision: revision)
    }

    private func enqueue(_ next: LibraryBrowseRequest, revision: String?, delay: Duration = .zero) {
        epoch &+= 1; task?.cancel(); pageID = UUID(); filterInvalid = false
        if request != next { page = nil; observedAt = nil }
        request = next; isFresh = false; loading = true; problem = nil; restartRequired = false
        paginationRevision = revision
        pending = ReadIntent(request: next, revision: revision, epoch: epoch, delay: delay)
        pump()
    }

    private func pump() {
        guard active && !stopped, task == nil, let intent = pending else { return }
        pending = nil
        let reader = read
        task = Task { [weak self] in
            do {
                if intent.delay > .zero { try await Task.sleep(for: intent.delay) }
                try Task.checkCancellation()
                let value = try await reader(intent.request).validated(for: intent.request)
                try Task.checkCancellation()
                guard let self, self.active && !self.stopped && self.epoch == intent.epoch else {
                    self?.finish(); return
                }
                if let expected = intent.revision, value.revision != expected {
                    throw LibraryCatalogError.changedRevision
                }
                self.page = value; self.observedAt = self.now(); self.isFresh = true
                self.problem = nil; self.restartRequired = false; self.loading = false; self.pageID = UUID()
            } catch {
                if let self, self.active && !self.stopped && self.epoch == intent.epoch {
                    self.loading = false; self.isFresh = false
                    self.restartRequired = (error as? LibraryCatalogError) == .changedRevision
                    self.problem = error.localizedDescription
                }
            }
            self?.finish()
        }
    }
    private func finish() { task = nil; pump() }
}