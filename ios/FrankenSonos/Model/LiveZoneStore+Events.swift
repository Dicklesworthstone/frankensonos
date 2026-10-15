import Foundation

extension LiveZoneStore {
    func apply(_ event: DaemonEvent) async {
        if event.kind == "events.reset" {
            lastEventID = nil
            scheduleTopology(refreshing: true)
            return
        }
        if let id = event.id, UInt64(id) != nil { lastEventID = id }
        if event.kind == "topology.changed" {
            scheduleTopology()
            return
        }
        guard ["zone.state", "player.health"].contains(event.kind),
              let delta = try? JSONDecoder().decode(ZoneDeltaDTO.self, from: Data(event.data.utf8)),
              let rawRoom = delta.room else { return }
        let fields = (try? JSONSerialization.jsonObject(with: Data(event.data.utf8))) as? [String: Any]
        let trackChanged = fields?.keys.contains("track") == true
        let matches = roomRows.filter { $0.name == rawRoom || roomKey($0) == rawRoom }.map(roomKey)
        guard !matches.isEmpty else { scheduleTopology(); return }
        if event.kind == "player.health" {
            for room in matches {
                if delta.health == "offline" { offlineRooms.insert(room) }
                else if delta.health != nil { offlineRooms.remove(room) }
            }
            return
        }
        guard matches.count == 1, let room = matches.first else { scheduleTopology(); return }
        if let length = delta.queue_length { queueLengths[room] = length }
        if let transport = delta.transport { applyTransport(transport, room: room) }
        if let volume = delta.volume, volumeEdits[room] == nil {
            roomVolumes[room] = Double(volume) / 100
            rebuildZones()
        }
        if delta.volume != nil || trackChanged || delta.transport != nil || delta.group_volume != nil {
            if volumeEdits[room] != nil && !trackChanged && delta.transport == nil { return }
            let zoneRoom = zones.first(where: { $0.roomNames.contains(room) })?.roomNames.first ?? room
            scheduleState(room: delta.volume != nil ? room : zoneRoom)
        }
    }

    private func applyTransport(_ transport: String, room: String) {
        guard let index = zones.firstIndex(where: { $0.roomNames.contains(room) }) else { return }
        let zone = zones[index]
        let now = Date()
        if let base = positions[zone.id] {
            let elapsed = zone.isPlaying ? max(0, now.timeIntervalSince(base.at)) : 0
            positions[zone.id] = (base.seconds + elapsed, now)
        }
        zones[index].isPlaying = shownPlaying(zone: zone.id, reported: transport, current: zone.isPlaying)
        if let coordinator = zone.roomNames.first, let old = states[coordinator] {
            states[coordinator] = ZoneStateDTO(zone: old.zone, transport_state: transport, volume: old.volume, track: old.track)
        }
        tick(now: now)
    }

    func scheduleState(room: String) {
        guard refetchTasks[room] == nil else { return }
        let revision = lifecycleRevision
        refetchTasks[room] = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(150))
                guard let self else { return }
                try await self.refetch(room: room)
            } catch { }
            if self?.lifecycleRevision == revision { self?.refetchTasks[room] = nil }
        }
    }

    func scheduleTopology(refreshing: Bool = false) {
        guard !grouping, topologyTask == nil else { return }
        if refreshing { connectionStatus = .refreshing }
        let revision = lifecycleRevision
        topologyTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(150))
                guard let self else { return }
                try await self.refreshAll()
                if refreshing && self.streamOpen { self.connectionStatus = .live }
            } catch {
                if !Task.isCancelled { self?.connectionStatus = .reconnecting }
            }
            if self?.lifecycleRevision == revision { self?.topologyTask = nil }
        }
    }
}
