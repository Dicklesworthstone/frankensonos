import Foundation

@main
@MainActor
enum MockChecks {
    static func main() {
        let store = MockZoneStore()
        func invariant(_ name: String) {
            let all = store.zones.flatMap(\.roomNames)
            precondition(all.count == 9 && Set(all) == Set(store.rooms), "Lost or duplicated rooms: \(name)")
            precondition(Set(store.zones.map(\.id)).count == store.zones.count, "Duplicate zone IDs: \(name)")
            precondition(store.zones.contains(where: { $0.id == store.selectedZoneID }), "Missing selection: \(name)")
        }
        invariant("initial")
        store.setRoomVolume(0.73, room: "Bedroom")
        let office = store.zone(for: "Office").id
        let patio = store.zone(for: "Patio").id
        store.group(patio, onto: office)
        invariant("drag group")
        precondition(store.zone(for: "Office").roomNames == ["Office", "Patio"])
        let kitchen = store.zone(for: "Kitchen").id
        store.setGroupedRooms(["Kitchen", "Living Room"], basedOn: kitchen)
        invariant("edit group")
        precondition(store.zone(for: "Office").roomNames == ["Office", "Patio"], "Unrelated group was split")
        precondition(store.roomVolumes["Bedroom"] == 0.73, "Device volume was lost")
        store.setGroupedRooms([], basedOn: kitchen)
        invariant("empty edit ignored")
        store.groupAll()
        invariant("party mode")
        precondition(store.zones.count == 1)
        store.pauseAll()
        precondition(store.zones.allSatisfy { !$0.isPlaying })
        store.ungroupAll()
        invariant("ungroup")
        precondition(store.zones.count == 9 && store.zone(for: "Bedroom").volume == 0.73)
        store.selectedZoneID = store.zone(for: "Movie Room").id
        store.selectTrack(store.tracks[2], in: store.selectedZoneID)
        precondition(store.selectedZone.track.id == 3 && store.selectedZone.isPlaying)
        let timerZone = store.selectedZoneID
        let start = Date(timeIntervalSince1970: 1000)
        store.setSleepTimer(15, now: start)
        store.selectedZoneID = store.zone(for: "Office").id
        store.selectTrack(store.tracks[1], in: store.selectedZoneID)
        store.tick(now: start.addingTimeInterval(899))
        precondition(store.zones.first(where: { $0.id == timerZone })!.isPlaying)
        store.tick(now: start.addingTimeInterval(900))
        precondition(!store.zones.first(where: { $0.id == timerZone })!.isPlaying)
        precondition(store.selectedZone.isPlaying && store.sleepMinutes == nil)
        for index in 0..<40 {
            let target = store.zones[index % store.zones.count].id
            store.setGroupedRooms(store.rooms.enumerated().filter { ($0.offset + index) % 3 == 0 }.map(\.element), basedOn: target)
            invariant("repeated regroup \(index)")
        }
        print("PASS: room inventory, unique IDs, selected target, unrelated groups, per-room volume, party mode, pause all, ungroup, track selection, sleep timer target/expiry, 40 regroup operations")
    }
}
