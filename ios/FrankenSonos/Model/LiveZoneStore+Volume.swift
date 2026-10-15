import Foundation

extension LiveZoneStore {
    func beginVolume(room: String) { draggingRooms.insert(room) }
    func finishVolume(room: String) { draggingRooms.remove(room) }

    func setVolume(_ value: Double, for zoneID: UUID) {
        guard let zone = zones.first(where: { $0.id == zoneID }) else { return }
        for room in zone.roomNames { setRoomVolume(value, room: room) }
    }
    func setRoomVolume(_ value: Double, room: String) {
        guard value.isFinite, !offlineRooms.contains(room) else { return }
        let level = min(1, max(0, value))
        if var edit = volumeEdits[room] {
            edit.level = level
            edit.revision += 1
            edit.editedAt = Date()
            volumeEdits[room] = edit
        } else {
            volumeEdits[room] = VolumeEdit(original: roomVolumes[room], level: level, revision: 1, editedAt: Date())
        }
        roomVolumes[room] = level
        rebuildZones()
        guard volumeTasks[room] == nil else { return }
        volumeTasks[room] = Task { [weak self] in await self?.writeVolume(room: room) }
    }

    private func writeVolume(room: String) async {
        let revision = lifecycleRevision
        defer { if lifecycleRevision == revision { volumeTasks[room] = nil } }
        while !Task.isCancelled, let edit = volumeEdits[room] {
            do {
                let delay = max(0, 0.1 - Date().timeIntervalSince(edit.editedAt))
                if delay > 0 {
                    try await Task.sleep(for: .seconds(delay))
                    continue
                }
                if edit.acknowledged == edit.revision {
                    if draggingRooms.contains(room) {
                        try await Task.sleep(for: .milliseconds(50))
                        continue
                    }
                    let state = try? await client.state(room: target(room))
                    try Task.checkCancellation()
                    guard volumeEdits[room]?.revision == edit.revision, !draggingRooms.contains(room) else { continue }
                    volumeEdits[room] = nil
                    if let state { accept(state: state, room: room) }
                    else { scheduleState(room: room) }
                    return
                }
                try await client.command("volume", body: ["zone": target(room), "volume": Int((edit.level * 100).rounded())])
                try Task.checkCancellation()
                if volumeEdits[room]?.revision == edit.revision { volumeEdits[room]?.acknowledged = edit.revision }
            } catch {
                if Task.isCancelled { return }
                if volumeEdits[room]?.revision == edit.revision {
                    roomVolumes[room] = edit.original
                    volumeEdits[room] = nil
                    rebuildZones()
                    showFailure(error)
                    if ((error as? DaemonFailure)?.status ?? 500) >= 500 { try? await refetch(room: room) }
                    return
                }
            }
        }
    }
}
