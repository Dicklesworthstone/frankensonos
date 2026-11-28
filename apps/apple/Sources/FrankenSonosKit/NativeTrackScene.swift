import Foundation

extension SonosPlaylistTracksSelection {
    /// The saved DTO the daemon must return after capturing this exact native
    /// selection. Titles and resources remain byte-exact, including repeats.
    func sceneSource() throws -> Scene.Source {
        let tracks = try choices.map { choice -> Scene.Track in
            guard let uri = choice.item.uri else {
                throw ContractError.invalid("A selected native occurrence has no playable resource.")
            }
            let title = choice.item.title.unicodeScalars.contains(where: { !$0.properties.isWhitespace })
                ? choice.item.title : nil
            return Scene.Track(sourceUri: uri, title: title)
        }
        let source = Scene.Source(kind: "sonos_playlist_tracks", name: playlist.id, uri: nil, mood: nil,
            playlist: .init(household: zone.household, id: playlist.id, title: playlist.title), tracks: tracks)
        try source.validateNativeTracks()
        return source
    }

    /// Only the original source and reviewed occurrence revision authorize
    /// capture. Native URI/DIDL fields are never authored by this client.
    var sceneRequest: JSONValue {
        .object([
            "kind": .string("sonos_playlist_tracks"), "name": .string(playlist.id),
            "selection": .object([
                "zone": .string(zone.id.address),
                "positions": .array(positions.map { .integer(Int64($0)) }),
                "expected_revision": .string(revision)
            ])
        ])
    }
}

extension SceneDraft {
    public init(nativeSelection: SonosPlaylistTracksSelection, destinations: [Zone]) throws {
        _ = try nativeSelection.sceneSource()
        guard !destinations.isEmpty,
              destinations.allSatisfy({ PlaylistContentsRequest.exact($0.household, nativeSelection.zone.household) }) else {
            throw ContractError.invalid("Choose scene groups in the native playlist's original household.")
        }
        _ = try Zone.validate(destinations)
        self.init()
        groups = destinations.map { zone in
            var group = SceneGroupDraft()
            group.coordinator = zone.id
            group.members = zone.rooms.subtracting([zone.id])
            group.source = .nativeTracks
            group.playback = .playing
            group.nativeSelection = nativeSelection
            return group
        }
    }
}

extension SceneDefinition {
    /// Normal authoring fields are unchanged; native previews are replaced by
    /// their capture request only at the reviewed write boundary.
    func requestGroups() throws -> [JSONValue] {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        var fields = try JSONDecoder().decode([JSONValue].self, from: encoder.encode(groups))
        for (index, selection) in nativeSelections {
            guard fields.indices.contains(index), case .object(var group) = fields[index],
                  try selection.sceneSource() == groups[index].source else {
                throw ContractError.invalid("The native selection no longer matches its reviewed scene source.")
            }
            group["source"] = selection.sceneRequest
            fields[index] = .object(group)
        }
        return fields
    }

    func validateNativeSelections(client: DaemonClient, zones: [Zone]) async throws {
        guard !nativeSelections.isEmpty else { return }
        var seen: [SonosPlaylistTracksSelection] = []
        for index in nativeSelections.keys.sorted() {
            guard let selection = nativeSelections[index] else { continue }
            if seen.contains(selection) { continue }
            try Task.checkCancellation()
            guard zones.contains(where: { selection.zone.sameGroup(as: $0) }) else {
                throw ContractError.invalid("The native playlist's source group changed. Choose its occurrences again.")
            }
            try await selection.validate(using: { try await client.sonosPlaylistContents($0) })
            seen.append(selection)
        }
        // Source reads can span several pages. Keep the original source group
        // and every affected destination together across those reads.
        let current = try await client.zones()
        try Task.checkCancellation()
        let relevant = zones.filter { zone in
            zone.rooms.contains(where: { roomAddresses.contains($0.address) })
                || seen.contains(where: { $0.zone.sameGroup(as: zone) })
        }
        guard relevant.allSatisfy({ before in current.contains(where: { before.sameGroup(as: $0) }) }) else {
            throw ContractError.invalid("A native source or scene destination group changed while reading the selected tracks. Nothing was sent.")
        }
    }
}

extension DaemonClient {
    /// A room-set destination becomes the exact groups displayed in the scene
    /// composer. Later alias edits do not silently retarget the saved scene.
    func nativeSceneDestinations(_ selection: SonosPlaylistTracksSelection) async throws -> [Zone] {
        let zones = try await self.zones()
        guard zones.contains(where: { selection.zone.sameGroup(as: $0) }) else {
            throw ContractError.invalid("The playlist source group changed. Read it again before saving a scene.")
        }
        let destinations: [Zone]
        if let target = selection.target {
            let audience = try await get(SonosPlaylistAudience.self, ["rooms", "resolve"],
                query: [.init(name: "target", value: target)])
            destinations = try audience.zones(for: selection.zone, target: target, in: zones)
        } else {
            destinations = [selection.zone]
        }
        try await selection.validate(using: { try await self.sonosPlaylistContents($0) })
        let current = try await self.zones()
        try Task.checkCancellation()
        guard ([selection.zone] + destinations).allSatisfy({ before in
            current.contains(where: { before.sameGroup(as: $0) })
        }) else {
            throw ContractError.invalid("The native source or chosen scene groups changed. Choose the selection again.")
        }
        return destinations
    }
}
