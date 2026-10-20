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
    case invalidQuery, invalidPage, changedRevision
    public var errorDescription: String? {
        switch self {
        case .invalidQuery: "Use a filter of at most 512 UTF-8 bytes without control characters."
        case .invalidPage: "The cached library page is incomplete or inconsistent. Refresh the listing."
        case .changedRevision: "Your library changed while browsing. Refresh from the beginning; pages from different versions were not combined."
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
    public var id: String { sourceUri }
}

public struct LibraryCatalogPage: Decodable, Equatable, Sendable {
    public let items: [LibraryCatalogItem]
    public let total: UInt32
    public let offset: UInt32
    public let limit: UInt32
    public let nextOffset: UInt32?
    public let kind: LibraryCategory
    public let albumUri: String?
    public let revision: String
    public let coverage: String

    /// Never join pages across revisions or guess identities from titles.
    public func validated(for request: LibraryBrowseRequest) throws -> Self {
        let count = min(LibraryBrowseRequest.pageSize, total > offset ? total - offset : 0)
        let end = UInt64(offset) + UInt64(items.count)
        let expectedNext = end < UInt64(total) ? UInt32(exactly: end) : nil
        guard kind == request.category, offset == request.offset,
              limit == LibraryBrowseRequest.pageSize, albumUri == request.albumUri,
              coverage == "cached", !revision.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              revision.utf8.count <= 256, items.count == Int(count), nextOffset == expectedNext,
              Set(items.map(\.id)).count == items.count else { throw LibraryCatalogError.invalidPage }
        for item in items {
            guard item.kind == kind.itemKind, LibraryBrowseRequest.source(item.sourceUri, kind: item.kind),
                  request.albumUri == nil || item.albumUri == request.albumUri,
                  kind != .albums || (item.cachedTracks ?? 0) > 0 else { throw LibraryCatalogError.invalidPage }
        }
        return self
    }
}

/// A UI selection is tied to a particular accepted page, not a mutable row index.
public struct LibraryCatalogSelection: Equatable, Sendable {
    public let item: LibraryCatalogItem
    fileprivate let pageID: UUID
}

/// A bounded, read-only browser. Playback stays with CommandReviewSession.
/// At most one read runs; an obsolete/cancelled reader must drain before the
/// newest navigation intent starts. A transport ignoring cancellation cannot
/// repaint, launch overlapping reads, or revive an earlier selection.
@MainActor public final class LibraryCatalogModel {
    public typealias Reader = @Sendable (LibraryBrowseRequest) async throws -> LibraryCatalogPage
    public var onChange: (@MainActor () -> Void)?
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
                read: @escaping Reader) {
        self.read = read; self.now = now; self.searchDelay = searchDelay
        // Known constants, not input from a server or user.
        request = try! LibraryBrowseRequest()
    }

    public var canChoose: Bool { active && !stopped && isFresh && !loading && page != nil && !restartRequired }
    public var canGoBack: Bool { canChoose && request.offset > 0 }
    public var canGoForward: Bool { canChoose && page?.nextOffset != nil }

    public func resume() {
        guard !active && !stopped else { return }
        active = true
        guard !restartRequired && !filterInvalid else { return }
        enqueue(request, revision: page?.revision ?? paginationRevision)
    }
    public func suspend() {
        active = false; epoch &+= 1; pending = nil; task?.cancel()
        isFresh = false; loading = false; pageID = UUID()
    }
    public func stop() { suspend(); stopped = true }

    /// Explicit refresh restarts at zero; never automatically splice a changed listing.
    public func refresh() {
        guard active && !stopped && !filterInvalid else { return }
        guard let first = try? LibraryBrowseRequest(category: request.category, query: request.query,
                                                   albumUri: request.albumUri) else { return }
        enqueue(first, revision: nil)
    }

    public func browse(_ category: LibraryCategory) {
        guard active && !stopped else { return }
        album = nil; albumReturn = nil; filterInvalid = false
        guard let next = try? LibraryBrowseRequest(category: category, query: request.query) else { return }
        enqueue(next, revision: nil)
    }

    public func filter(_ text: String) {
        guard active && !stopped else { return }
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
        epoch &+= 1; task?.cancel(); pageID = UUID()
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
