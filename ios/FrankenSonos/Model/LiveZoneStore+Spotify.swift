import Foundation

extension LiveZoneStore {
    /// Play a Spotify album, track or playlist (`spotify:...`) in the selected room's group. Like a tap on play it shows
    /// as playing at once and holds that while the speaker catches up; a refusal puts the room back and says why.
    func playSpotify(uri: String, title: String) {
        let zoneID = selectedZoneID
        guard let index = zones.firstIndex(where: { $0.id == zoneID }), let room = coordinator(zoneID), !offlineRooms.contains(room) else { return }
        let original = zones[index].isPlaying
        zones[index].isPlaying = true
        holdTransport(zoneID, playing: true)
        send("play", room: room, extra: ["source_uri": uri, "title": title], confirm: zoneID) { [weak self] in
            guard let self, let index = self.zones.firstIndex(where: { $0.id == zoneID }) else { return }
            self.transportIntents[zoneID] = nil
            self.zones[index].isPlaying = original
        }
    }

    /// `start`, `skip` or `stop` the classical DJ in the selected room's group. The daemon explains a refusal (for
    /// example that Spotify is not signed in); it arrives through the usual command error.
    func dj(_ action: String) {
        let zoneID = selectedZoneID
        guard ["start", "skip", "stop"].contains(action), let room = coordinator(zoneID), !offlineRooms.contains(room) else { return }
        if action == "start" { holdTransport(zoneID, playing: true) }
        if action == "stop" { holdTransport(zoneID, playing: false) }
        send("dj/\(action)", room: room, confirm: action == "skip" ? nil : zoneID) { [weak self] in
            self?.transportIntents[zoneID] = nil
        }
    }
}
