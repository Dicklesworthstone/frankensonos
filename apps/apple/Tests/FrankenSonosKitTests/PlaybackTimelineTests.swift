import Foundation
import Testing
@testable import FrankenSonosKit

private let timelineDate = Date(timeIntervalSince1970: 1_000)
private func timelineTrack(duration: UInt32? = 240, position: UInt32? = 30, uri: String = "test:track:one",
                           title: String = "Test recording", creator: String = "Test artist", album: String = "Test album") -> Track {
    Track(uri: uri, title: title, creator: creator, album: album,
          durationSecs: duration, positionSecs: position, queuePosition: 1)
}

@Test func timelineAdvancesOnlyKnownFreshPlayingAudio() {
    let track = timelineTrack()
    let playing = PlaybackTimeline(track: track, isPlaying: true, observedAt: timelineDate, isFresh: true)
    let paused = PlaybackTimeline(track: track, isPlaying: false, observedAt: timelineDate, isFresh: true)
    let stale = PlaybackTimeline(track: track, isPlaying: true, observedAt: timelineDate, isFresh: false)
    let later = timelineDate.addingTimeInterval(15)
    #expect(playing.position(at: later) == 45)
    #expect(paused.position(at: later) == 30)
    #expect(stale.position(at: later) == 30)
    #expect(playing.canSeek && paused.canSeek && !stale.canSeek)
    #expect(playing.elapsedLabel(at: later) == "0:45")
    #expect(playing.remainingLabel(at: later) == "−3:15")
}

@Test func timelineStopsAtTrackEndAndHandlesClockChanges() {
    let timeline = PlaybackTimeline(track: timelineTrack(), isPlaying: true, observedAt: timelineDate, isFresh: true)
    #expect(timeline.position(at: timelineDate.addingTimeInterval(10_000)) == 240)
    #expect(timeline.position(at: timelineDate.addingTimeInterval(-10_000)) == 30)
    #expect(timeline.position(at: Date(timeIntervalSince1970: .nan)) == 30)
    #expect(timeline.remainingLabel(at: timelineDate.addingTimeInterval(10_000)) == "−0:00")
}

@Test func timelineFreezesTheDisplayedEstimateDuringARefreshOrDisconnection() {
    let frozen = PlaybackTimeline(track: timelineTrack(), isPlaying: true, observedAt: timelineDate, isFresh: false,
                                  frozenAt: timelineDate.addingTimeInterval(15))
    #expect(frozen.position(at: timelineDate.addingTimeInterval(20)) == 45)
    #expect(frozen.position(at: timelineDate.addingTimeInterval(600)) == 45)
    #expect(!frozen.canSeek)
    let newerObservation = PlaybackTimeline(track: timelineTrack(position: 60), isPlaying: true,
                                           observedAt: timelineDate.addingTimeInterval(30), isFresh: false,
                                           frozenAt: timelineDate.addingTimeInterval(15))
    #expect(newerObservation.position(at: timelineDate.addingTimeInterval(600)) == 60)
}

@Test func timelineDoesNotInventAPositionForUnknownAudio() {
    let missing = PlaybackTimeline(track: nil, isPlaying: false, observedAt: timelineDate, isFresh: true)
    let unknown = PlaybackTimeline(track: timelineTrack(position: nil), isPlaying: true, observedAt: timelineDate, isFresh: true)
    for timeline in [missing, unknown] {
        #expect(timeline.position(at: timelineDate) == nil)
        #expect(timeline.elapsedLabel(at: timelineDate) == "–:––")
        #expect(timeline.remainingLabel(at: timelineDate) == "–:––")
        #expect(!timeline.canSeek)
        #expect(timeline.seekPosition(fraction: 0.5) == nil)
    }
}

@Test func timelineDoesNotEstimateAStreamWithUnknownDuration() {
    for duration: UInt32? in [nil, 0] {
        let stream = PlaybackTimeline(track: timelineTrack(duration: duration), isPlaying: true,
                                      observedAt: timelineDate, isFresh: true)
        #expect(stream.duration == nil && !stream.canSeek)
        #expect(stream.position(at: timelineDate.addingTimeInterval(600)) == 30)
        #expect(stream.remainingLabel(at: timelineDate) == "–:––")
    }
}

@Test func timelineClampsReportedPositionToTheKnownDuration() {
    let timeline = PlaybackTimeline(track: timelineTrack(duration: 10, position: 30), isPlaying: false,
                                    observedAt: timelineDate, isFresh: true)
    #expect(timeline.position(at: timelineDate) == 10)
}

@Test func timelineSeekingClampsToTheLastPlayableSecond() {
    let timeline = PlaybackTimeline(track: timelineTrack(duration: 240), isPlaying: true,
                                    observedAt: timelineDate, isFresh: true)
    #expect(timeline.seekPosition(fraction: -1) == 0)
    #expect(timeline.seekPosition(fraction: 0.5) == 120)
    #expect(timeline.seekPosition(fraction: 1) == 239)
    #expect(timeline.seekPosition(fraction: 2) == 239)
    #expect(timeline.seekPosition(fraction: .nan) == nil)
    #expect(timeline.seekPosition(fraction: .infinity) == nil)
    let short = PlaybackTimeline(track: timelineTrack(duration: 1, position: 0), isPlaying: true,
                                 observedAt: timelineDate, isFresh: true)
    #expect(short.seekPosition(fraction: 1) == 0)
    let stopped = PlaybackTimeline(track: timelineTrack(), isPlaying: false,
                                   observedAt: timelineDate, isFresh: true, allowsSeeking: false)
    #expect(!stopped.canSeek && stopped.seekPosition(fraction: 0) == nil)
}

@Test func timelineFormatsLongWorksAndRejectsNonFiniteTimes() {
    #expect(PlaybackTimeline.format(0) == "0:00")
    #expect(PlaybackTimeline.format(65.9) == "1:05")
    #expect(PlaybackTimeline.format(3_661) == "1:01:01")
    #expect(PlaybackTimeline.format(-20) == "0:00")
    #expect(PlaybackTimeline.format(.nan) == "–:––")
    #expect(PlaybackTimeline.format(.infinity) == "–:––")
    #expect(PlaybackTimeline.format(TimeInterval(UInt32.max) + 1) == "1193046:28:15")
}

@Test func artworkUsesOnlyTheDaemonOriginAndHouseholdQualifiedRoom() throws {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test:8443"))
    let zone = Zone(household: "TEST-A", coordinatorRoom: "Music & Study/One", members: ["Music & Study/One"], transportState: "playing")
    let track = timelineTrack(uri: "https://untrusted.example.test/image?token=not-a-real-token")
    let url = try #require(client.artworkURL(in: zone, track: track))
    let parts = try #require(URLComponents(url: url, resolvingAgainstBaseURL: false))
    #expect(parts.scheme == "https" && parts.host == "music.example.test" && parts.port == 8443)
    #expect(parts.path == "/art")
    #expect(parts.queryItems?.first(where: { $0.name == "zone" })?.value == "Music & Study/One@TEST-A")
    let tag = try #require(parts.queryItems?.first(where: { $0.name == "v" })?.value)
    #expect(tag.count <= 64 && tag.allSatisfy { $0.isASCII && $0.isHexDigit })
    #expect(!url.absoluteString.contains("untrusted") && !url.absoluteString.contains("token"))
    #expect(client.artworkURL(in: reviewZone(house: "TEST-A"), track: track) !=
            client.artworkURL(in: reviewZone(house: "TEST-B"), track: track))
}

@Test func artworkCacheChangesForRecordingMetadataButNotElapsedTime() throws {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test")), zone = reviewZone()
    let first = try #require(client.artworkURL(in: zone, track: timelineTrack()))
    #expect(first == client.artworkURL(in: zone, track: timelineTrack(position: 120)))
    #expect(first != client.artworkURL(in: zone, track: timelineTrack(uri: "test:track:two")))
    #expect(first != client.artworkURL(in: zone, track: timelineTrack(title: "Next radio song")))
    #expect(first != client.artworkURL(in: zone, track: timelineTrack(creator: "Another artist")))
    #expect(first != client.artworkURL(in: zone, track: timelineTrack(album: "Another album")))
    #expect(client.artworkURL(in: zone, track: nil) == nil)
    #expect(client.artworkURL(in: zone, track: timelineTrack(uri: "")) == nil)
    #expect(client.artworkURL(in: zone, track: timelineTrack(uri: "NOT_IMPLEMENTED")) == nil)
}
