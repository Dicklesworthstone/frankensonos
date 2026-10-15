import Foundation

extension LiveZoneStore {
    func togglePlayback(for zoneID: UUID) {
        guard let index = zones.firstIndex(where: { $0.id == zoneID }), let room = coordinator(zoneID), !offlineRooms.contains(room) else { return }
        let original = zones[index].isPlaying
        let desired = !original
        zones[index].isPlaying = desired
        holdTransport(zoneID, playing: desired)
        send(desired ? "resume" : "pause", room: room, confirm: zoneID) { [weak self] in
            guard let self, let index = self.zones.firstIndex(where: { $0.id == zoneID }) else { return }
            self.transportIntents[zoneID] = nil
            self.zones[index].isPlaying = original
        }
    }

    /// Remember that the user asked for `playing`, so reports that lag behind the speaker do not undo the tap.
    func holdTransport(_ zoneID: UUID, playing: Bool) {
        transportIntents[zoneID] = (playing, Date().addingTimeInterval(Self.holdSeconds))
    }

    /// Whether a zone is shown as playing, given what the speaker reports. After a tap on play a Sonos reports the old
    /// state for a moment, then `transitioning` (measured: about 0.1 s, then `playing` at about 0.65 s on a good link, more
    /// over a slow one). Reading `transitioning` as "not playing" made the button fall back to paused and then return.
    /// During the hold a report that disagrees with the tap is ignored; once it agrees, or the hold ends, the report wins.
    /// With no tap to honor, `transitioning` keeps what is shown instead of flashing the other state.
    func shownPlaying(zone zoneID: UUID, reported: String, current: Bool?, now: Date = Date()) -> Bool {
        let reportedPlaying = reported == "playing"
        if let intent = transportIntents[zoneID] {
            if now >= intent.until || reportedPlaying == intent.playing && reported != "transitioning" {
                transportIntents[zoneID] = nil
                return reportedPlaying
            }
            return intent.playing
        }
        if reported == "transitioning", let current { return current }
        return reportedPlaying
    }
    func pauseAll() {
        for zone in zones where zone.isPlaying { togglePlayback(for: zone.id) }
    }
    func advanceTrack(in zoneID: UUID, direction: Int) {
        guard let room = coordinator(zoneID) else { return }
        send(direction < 0 ? "previous" : "next", room: room)
    }
    func selectTrack(_ track: Track, in zoneID: UUID) {
        guard let index = zones.firstIndex(where: { $0.id == zoneID }), let room = coordinator(zoneID), let favorite = track.favoriteID else { return }
        guard let row = roomRows.first(where: { roomKey($0) == room }), row.household == track.household else {
            commandError = "Choose a favorite from this room's household."
            return
        }
        let original = zones[index]
        zones[index].track = track
        zones[index].isPlaying = true
        holdTransport(zoneID, playing: true)
        positions[zoneID] = (0, Date())
        elapsed = 0
        send("play/favorite", room: room, extra: ["favorite": favorite], confirm: zoneID) { [weak self] in
            guard let self, let index = self.zones.firstIndex(where: { $0.id == zoneID }) else { return }
            self.transportIntents[zoneID] = nil
            self.zones[index] = original
        }
    }

    /// `confirm` names a zone whose transport the user just changed: its state is re-read a few times until the speaker
    /// agrees, because the live stream may be off or late and the speaker takes a moment to settle.
    func send(_ path: String, room: String, extra: [String: Any] = [:], confirm: UUID? = nil, revert: @escaping () -> Void = {}) {
        guard !offlineRooms.contains(room) else { revert(); return }
        let id = UUID()
        let outcome = CommandOutcome()
        // The POSTs for one room run in the order they were tapped: independent tasks could reach the daemon out of
        // order, and a start tapped before a stop must not land after it. Only the POST is chained, so a command never
        // waits for an earlier one's confirmation polling.
        let previous = commandChain[room]
        let post = Task { [weak self] in
            await previous?.value
            guard let self, !Task.isCancelled else { return }
            do {
                var body = extra
                body["zone"] = self.target(room)
                try await self.client.command(path, body: body)
                outcome.sent = true
            } catch {
                guard !Task.isCancelled else { return }
                revert()
                self.showFailure(error)
                if ((error as? DaemonFailure)?.status ?? 500) >= 500 { try? await self.refetch(room: room) }
            }
        }
        commandChain[room] = post
        commandTasks[id] = Task { [weak self] in
            await post.value
            guard let self else { return }
            defer { self.commandTasks[id] = nil }
            guard outcome.sent, !Task.isCancelled else { return }
            do { try await self.refetch(room: room) }
            catch { if !Task.isCancelled { self.scheduleState(room: room) } }
            guard let zoneID = confirm else { return }
            for delay in [0.5, 1.0, 1.5, 2.5] {
                guard self.transportIntents[zoneID] != nil else { return }
                do { try await Task.sleep(for: .seconds(delay)) } catch { return }
                try? await self.refetch(room: room)
            }
        }
    }

    func setGroupedRooms(_ roomNames: [String], basedOn zoneID: UUID) {
        if grouping {
            queuedGrouping = (roomNames, zoneID)
            return
        }
        guard let source = zones.first(where: { $0.id == zoneID }),
              let coordinator = source.roomNames.first, !roomNames.isEmpty else { return }
        let selected = Array(Set(roomNames)).sorted()
        let households = Set(selected.compactMap { name in roomRows.first(where: { roomKey($0) == name })?.household })
        guard households.count <= 1 else {
            commandError = "Rooms from different households cannot share a group."
            return
        }
        let leader = selected.contains(coordinator) ? coordinator : selected[0]
        let oldZones = zones
        let oldSelection = selectedZoneID
        grouping = true
        topologyTask?.cancel()
        topologyTask = nil
        optimisticGroup(selected, leader: leader, source: source)
        let id = UUID()
        let revision = lifecycleRevision
        commandTasks[id] = Task { [weak self] in
            guard let self else { return }
            defer {
                self.commandTasks[id] = nil
                if self.lifecycleRevision == revision {
                    self.grouping = false
                    if let next = self.queuedGrouping {
                        self.queuedGrouping = nil
                        self.setGroupedRooms(next.rooms, basedOn: next.zoneID)
                    }
                }
            }
            do {
                for room in source.roomNames where !selected.contains(room) {
                    try await self.client.command("ungroup", body: ["zone": self.target(room)])
                }
                if leader != coordinator { try await self.client.command("ungroup", body: ["zone": self.target(leader)]) }
                for room in selected where room != leader {
                    try await self.client.command("group", body: ["zone": self.target(room), "to": self.target(leader)])
                }
                try await self.refreshAll(afterGrouping: true)
            } catch {
                guard !Task.isCancelled else { return }
                self.zones = oldZones
                self.selectedZoneID = oldSelection
                self.showFailure(error)
                try? await self.refreshAll(afterGrouping: true)
            }
        }
    }

    func groupAll() {
        guard let room = coordinator(selectedZoneID), let household = roomRows.first(where: { roomKey($0) == room })?.household else { return }
        setGroupedRooms(roomRows.filter { $0.household == household }.map(roomKey), basedOn: selectedZoneID)
    }

    func ungroupAll() {
        guard !grouping else { return }
        let oldZones = zones
        grouping = true
        topologyTask?.cancel()
        topologyTask = nil
        zones = oldZones.flatMap { zone in
            zone.roomNames.map { name in
                let row = roomRows.first(where: { roomKey($0) == name })
                let id = StableIdentity.zone(room: row?.name ?? name, household: row?.household ?? "")
                return AudioZone(id: id, roomNames: [name], track: zone.track, isPlaying: zone.isPlaying, volume: roomVolumes[name] ?? 0)
            }
        }
        let roomsToDetach = oldZones.flatMap { Array($0.roomNames.dropFirst()) }
        let id = UUID()
        let revision = lifecycleRevision
        commandTasks[id] = Task { [weak self] in
            guard let self else { return }
            defer {
                self.commandTasks[id] = nil
                if self.lifecycleRevision == revision { self.grouping = false }
            }
            do {
                for room in roomsToDetach { try await self.client.command("ungroup", body: ["zone": self.target(room)]) }
                try await self.refreshAll(afterGrouping: true)
            } catch {
                guard !Task.isCancelled else { return }
                self.zones = oldZones
                self.showFailure(error)
                try? await self.refreshAll(afterGrouping: true)
            }
        }
    }

    private func optimisticGroup(_ selected: [String], leader: String, source: AudioZone) {
        let household = roomRows.first(where: { roomKey($0) == leader })?.household ?? ""
        let name = roomRows.first(where: { roomKey($0) == leader })?.name ?? leader
        let id = StableIdentity.zone(room: name, household: household)
        let members = [leader] + selected.filter { $0 != leader }
        var result = [AudioZone(id: id, roomNames: members, track: source.track, isPlaying: source.isPlaying, volume: source.volume)]
        for zone in zones {
            let remaining = zone.roomNames.filter { !selected.contains($0) }
            if zone.id == source.id {
                for room in remaining {
                    let row = roomRows.first(where: { roomKey($0) == room })
                    let roomID = StableIdentity.zone(room: row?.name ?? room, household: row?.household ?? "")
                    result.append(AudioZone(id: roomID, roomNames: [room], track: zone.track, isPlaying: zone.isPlaying, volume: roomVolumes[room] ?? 0))
                }
            } else if !remaining.isEmpty {
                var kept = zone
                kept.roomNames = remaining
                result.append(kept)
            }
        }
        zones = result
        selectedZoneID = id
    }
}

/// Whether a command's POST reached the daemon, shared between the task that sends it and the one that follows up.
final class CommandOutcome {
    var sent = false
}
