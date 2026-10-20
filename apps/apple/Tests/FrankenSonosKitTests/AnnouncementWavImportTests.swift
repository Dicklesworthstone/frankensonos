import Foundation
import Testing
@testable import FrankenSonosKit

private func wavGroup(_ household: String = "S2", members: [String] = ["Studio", "Hall"]) -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: members, transportState: "playing")
}
private func wavForm() -> AnnouncementForm {
    var form = AnnouncementForm(group: wavGroup()); form.selectAll(); return form
}

@Test func announcementFileSelectionRequiresSuccessfulReadAndCannotUseOldBytes() throws {
    var form = wavForm()
    form.kind = .wav
    #expect(throws: ContractError.self) { try form.announcement(in: wavGroup()) }
    let clip = try AnnouncementWav(data: announcementAudio())
    let id = form.beginFileImport()
    #expect(form.fileLoading)
    #expect(throws: ContractError.self) { try form.announcement(in: wavGroup()) }
    let accepted1 = (form.finishFileImport(clip, id: id))
    #expect(accepted1)
    let reviewed = try form.announcement(in: wavGroup())
    #expect(reviewed.source == .wav(clip))
    let replacement = form.beginFileImport()
    #expect(form.wav == nil && form.fileLoading)
    let accepted2 = (form.failFileImport(id: replacement))
    #expect(accepted2)
    #expect(throws: ContractError.self) { try form.announcement(in: wavGroup()) }
    #expect(reviewed.source == .wav(clip)) // A retained review is a value, not a file URL.
}

@Test func announcementFileSelectionRejectsLateCompletionsAndLateFailures() throws {
    var form = wavForm()
    let clip = try AnnouncementWav(data: announcementAudio())
    let old = form.beginFileImport(), latest = form.beginFileImport()
    let accepted3 = (!form.finishFileImport(clip, id: old))
    #expect(accepted3)
    let accepted4 = (!form.failFileImport(id: old))
    #expect(accepted4)
    #expect(form.fileLoading && form.wav == nil)
    let accepted5 = (form.finishFileImport(clip, id: latest))
    #expect(accepted5)
    let accepted6 = (!form.finishFileImport(clip, id: latest))
    #expect(accepted6)
    let accepted7 = (!form.failFileImport(id: old) && form.wav == clip)
    #expect(accepted7)
}

@Test func announcementFileCancellationSourceAndGroupChangesFencePendingReads() throws {
    let clip = try AnnouncementWav(data: announcementAudio())
    for action in 0..<4 {
        var form = wavForm()
        let id = form.beginFileImport()
        switch action {
        case 0: form.cancelFileImport()
        case 1: form.removeFile()
        case 2: form.kind = .chime
        default: form.observe(wavGroup("S1"))
        }
        let accepted8 = (!form.finishFileImport(clip, id: id))
        #expect(accepted8)
        #expect(!form.fileLoading && form.wav == nil)
    }
}

@Test func announcementFileGroupChangesRetainLoadedAudioButNeverRecipients() throws {
    var form = wavForm()
    let clip = try AnnouncementWav(data: announcementAudio())
    let id = form.beginFileImport();
    let accepted9 = (form.finishFileImport(clip, id: id))
    #expect(accepted9)
    form.observe(wavGroup(members: ["Hall", "Studio"]))
    #expect(form.selectedRooms.count == 2 && form.wav == clip)
    form.observe(wavGroup("S1"))
    #expect(form.selectedRooms.isEmpty && form.wav == clip)
    #expect(throws: ContractError.self) { try form.announcement(in: wavGroup("S1")) }
    form.select("Studio", selected: true)
    let new = try form.announcement(in: wavGroup("S1"))
    #expect(try new.fields(in: wavGroup("S1"))["rooms"] == .array([.string("Studio@S1")]))
}

@Test func announcementFileModesNeverMixSourcesOrStartPlaybackDuringSelection() throws {
    var form = wavForm(); form.speech = "Keep this message"
    let clip = try AnnouncementWav(data: announcementAudio())
    let id = form.beginFileImport();
    let accepted10 = (form.finishFileImport(clip, id: id))
    #expect(accepted10)
    for kind in AnnouncementForm.Kind.allCases {
        form.kind = kind
        let body = try form.announcement(in: wavGroup()).fields(in: wavGroup())
        #expect(["text", "chime", "wav_base64"].filter { body[$0] != nil }.count == 1)
        #expect(body.count == 3)
    }
    form.removeFile(); form.kind = .speech
    #expect(form.speech == "Keep this message")
}

@Test func announcementFileLoadsRealLocalBytesAndDoesNotRereadAfterEditing() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let url = dir.appendingPathComponent("例.wav")
    let data = announcementAudio(frames: 100)
    try data.write(to: url)
    let clip = try await AnnouncementWav.load(from: url)
    try announcementAudio(frames: 200).write(to: url)
    #expect(clip.name == "例.wav" && clip.frames == 100)
    #expect(Data(base64Encoded: clip.base64) == data)
    #expect(try await AnnouncementWav.load(from: url).frames == 200)
}

@Test func announcementFileRejectsDirectoriesMissingRemoteInvalidAndOversizedFiles() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let bad = dir.appendingPathComponent("bad.wav"), large = dir.appendingPathComponent("large.wav")
    try Data("not audio".utf8).write(to: bad)
    try Data(repeating: 0, count: AnnouncementWav.maximumBytes + 1).write(to: large)
    let remote = try #require(URL(string: "https://example.invalid/clip.wav"))
    for url in [dir, bad, large, dir.appendingPathComponent("missing.wav"), remote] {
        await #expect(throws: ContractError.self) { try await AnnouncementWav.load(from: url) }
    }
}

@Test func announcementFileAlreadyCancelledTaskNeverReads() async throws {
    let missing = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let task = Task {
        withUnsafeCurrentTask { $0?.cancel() }
        return try await AnnouncementWav.load(from: missing)
    }
    await #expect(throws: CancellationError.self) { try await task.value }
}
