import Foundation

extension LiveZoneStore {
    func refreshAll(afterGrouping: Bool = false) async throws {
        snapshotRevision += 1
        let revision = snapshotRevision
        async let fetchedZones = client.zones()
        async let fetchedRooms = client.rooms()
        let (newZones, newRooms) = try await (fetchedZones, fetchedRooms)
        var newStates: [String: ZoneStateDTO] = [:]
        var unavailable: Set<String> = []
        try await withThrowingTaskGroup(of: (String, ZoneStateDTO?).self) { group in
            for room in newRooms {
                let label = newRooms.filter { $0.name == room.name }.count > 1 ? "\(room.name)@\(room.household)" : room.name
                group.addTask { [client] in
                    do { return (label, try await client.state(room: "\(room.name)@\(room.household)")) }
                    catch let failure as DaemonFailure where ["PLAYER_UNREACHABLE", "UPNP_FAULT", "NOT_COORDINATOR"].contains(failure.code ?? "") {
                        return (label, nil)
                    }
                }
            }
            for try await (name, state) in group {
                newStates[name] = state
                if state == nil { unavailable.insert(name) }
            }
        }
        try Task.checkCancellation()
        guard revision == snapshotRevision, !grouping || afterGrouping else { return }
        AppLog.shared.add("refresh", "\(newZones.count) zones, \(newRooms.count) rooms, \(unavailable.count) unreachable")
        zoneRows = newZones
        roomRows = newRooms
        rooms = newRooms.map(roomKey)
        states = newStates
        roomVolumes = roomVolumes.filter { rooms.contains($0.key) || volumeEdits[$0.key] != nil }
        for (room, state) in states where volumeEdits[room] == nil {
            if let volume = state.volume { roomVolumes[room] = Double(volume) / 100 }
        }
        offlineRooms = offlineRooms.intersection(rooms).union(unavailable)
        rebuildZones(resyncPosition: true)
        do { try await refreshFavorites() }
        catch { if !Task.isCancelled { showFailure(error) } }
    }

    func refreshFavorites() async throws {
        var fetched: [Track] = []
        let households = Set(roomRows.map(\.household)).sorted()
        for household in households {
            guard let room = roomRows.first(where: { $0.household == household }) else { continue }
            let favorites = try await client.favorites(room: "\(room.name)@\(household)")
            for favorite in favorites where favorite.kind != "unplayable" {
                var track = Track.live(title: favorite.title, artist: favorite.description ?? "", album: "Sonos Favorites", key: "\(household):\(favorite.id)", artURL: favorite.art_uri.flatMap { URL(string: $0, relativeTo: client.baseURL)?.absoluteURL })
                track.favoriteID = favorite.id
                track.household = household
                fetched.append(track)
            }
        }
        try Task.checkCancellation()
        tracks = fetched
    }

    func rebuildZones(resyncPosition: Bool = false) {
        let selectedRoom = zones.first(where: { $0.id == selectedZoneID })?.roomNames.first
        zones = zoneRows.map { row in
            let room = key(name: row.coordinator_room, household: row.household)
            let state = states[room] ?? row.members.compactMap { states[key(name: $0, household: row.household)] }.first
            let track = state?.track
            let id = StableIdentity.zone(room: row.coordinator_room, household: row.household)
            let members = row.members.map { key(name: $0, household: row.household) }
            let playing = shownPlaying(zone: id, reported: state?.transport_state ?? row.transport_state, current: zones.first(where: { $0.id == id })?.isPlaying)
            if resyncPosition || positions[id] == nil {
                positions[id] = (track?.position_secs ?? 0, Date())
            }
            let values = members.compactMap { roomVolumes[$0] }
            let volume = values.isEmpty ? 0 : values.reduce(0, +) / Double(values.count)
            let model = Track.live(title: track?.title ?? (track == nil ? "Nothing playing" : "Live"), artist: track?.creator ?? "", album: track?.album ?? "", key: track?.title ?? track?.uri ?? "stopped", duration: track?.duration_secs ?? 0, artURL: track?.art_url.flatMap { URL(string: $0, relativeTo: client.baseURL)?.absoluteURL })
            return AudioZone(id: id, roomNames: members, track: model, isPlaying: playing, volume: volume)
        }
        if !zones.contains(where: { $0.id == selectedZoneID }) {
            selectedZoneID = zones.first(where: { $0.roomNames.contains(selectedRoom ?? "") })?.id ?? zones.first?.id ?? selectedZoneID
        }
        tick(now: Date())
    }

    func refetch(room: String) async throws {
        let state = try await client.state(room: target(room))
        try Task.checkCancellation()
        accept(state: state, room: room)
    }

    func accept(state: ZoneStateDTO, room: String) {
        states[room] = state
        if volumeEdits[room] == nil, let volume = state.volume { roomVolumes[room] = Double(volume) / 100 }
        let coordinatorRoom = key(name: state.zone.coordinator_room, household: state.zone.household)
        states[coordinatorRoom] = state
        let id = StableIdentity.zone(room: state.zone.coordinator_room, household: state.zone.household)
        positions[id] = (state.track?.position_secs ?? 0, Date())
        rebuildZones()
    }
}
