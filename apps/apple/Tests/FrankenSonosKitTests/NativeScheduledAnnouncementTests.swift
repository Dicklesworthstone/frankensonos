import Foundation
import Testing
@testable import FrankenSonosKit

private func savedAnnouncementFixture(_ recipe: [String: JSONValue] = [
    "text": .string("Time to leave"), "voice": .string("en-US"),
    "rooms": .array([.string("Studio@TEST-A"), .string("Hall@TEST-A")]),
    "volume": .integer(25)
]) -> [String: JSONValue] {
    scheduleFixture(["action": .string("announce"), "announcement": .object(recipe),
                     "summary": .string("Scheduled announcement")], removing: ["zone", "mood"])
}

@Suite("Native scheduled announcement compatibility") struct NativeScheduledAnnouncementTests {
    @Test func recipesDecodeWithoutBlockingOlderScheduleActions() async throws {
        let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
        var old = scheduleFixture(); old["id"] = .integer(8)
        await transport.setRows([savedAnnouncementFixture(), old])
        let rows = try await client.schedules()
        #expect(rows.count == 2)
        #expect(rows[1].announcement == nil)
        let saved = rows[0]
        #expect(saved.announcement?.text == "Time to leave")
        #expect(saved.announcement?.voice == "en-US")
        #expect(saved.announcement?.volume == 25)
        #expect(saved.targetLabel == "Rooms: Studio@TEST-A, Hall@TEST-A")
        #expect(ScheduleCommand.change(saved, .remove).detail.contains("Say: Time to leave"))
        #expect(saved.fields["announcement"] == savedAnnouncementFixture()["announcement"])
        let all = try scheduleDecode(savedAnnouncementFixture(["chime": .string("bell"), "rooms": .array([])])).validated()
        #expect(all.targetLabel == "All discovered rooms")
        #expect(all.announcement?.summary.contains("35") == true)
        #expect(try ScheduleCommand.change(all, .pause).payload().body == ["id": .integer(7)])
    }

    @Test func invalidOrConflictingRecipesAreNeverActionable() throws {
        let bad: [[String: JSONValue]] = [
            ["rooms": .array([])],
            ["text": .string("hello"), "chime": .string("bell"), "rooms": .array([])],
            ["chime": .string("bell"), "voice": .string("en-US"), "rooms": .array([])],
            ["chime": .string("unknown"), "rooms": .array([])],
            ["text": .string(""), "rooms": .array([])],
            ["text": .string(String(repeating: "a", count: 1_001)), "rooms": .array([])],
            ["text": .string("hello"), "voice": .string("--flag"), "rooms": .array([])],
            ["text": .string("hello"), "rooms": .array([.string(" ")])],
            ["chime": .string("bell"), "rooms": .array([]), "volume": .integer(101)],
            ["chime": .string("bell"), "rooms": .array([]), "wav_base64": .string("ignored")]
        ]
        for recipe in bad {
            #expect(throws: (any Error).self) { try scheduleDecode(savedAnnouncementFixture(recipe)).validated() }
        }
        var wrongAction = savedAnnouncementFixture(); wrongAction["action"] = .string("pause")
        wrongAction["zone"] = .string("Studio@TEST-A")
        #expect(throws: ContractError.self) { try scheduleDecode(wrongAction).validated() }
        var conflicting = savedAnnouncementFixture(); conflicting["volume"] = .integer(20)
        #expect(throws: ContractError.self) { try scheduleDecode(conflicting).validated() }
    }

    @Test func exactRecipeParticipatesInReviewAndAcknowledgementComparisons() throws {
        let original = try scheduleDecode(savedAnnouncementFixture()).validated()
        let changes: [[String: JSONValue]] = [
            ["text": .string("Different reminder")], ["voice": .string("en-GB")],
            ["rooms": .array([.string("Studio@TEST-B")])], ["volume": .integer(0)]
        ]
        for change in changes {
            let value = try #require(original.fields["announcement"])
            guard case .object(var recipe) = value else { Issue.record("Missing announcement recipe"); return }
            recipe.merge(change) { _, new in new }
            let changed = try scheduleDecode(savedAnnouncementFixture(recipe)).validated()
            #expect(!original.sameDefinition(as: changed))
        }
    }

    @Test func managementWorksWithOfflineSpeakersAndDoesNotDeliverAudio() async throws {
        for change in ScheduleChange.allCases {
            let transport = ScheduleTestTransport()
            await transport.setRows([savedAnnouncementFixture()])
            await transport.setTopology([])
            let client = try scheduleClient(transport)
            let stored = try #require(try await client.schedules().first)
            let session = CommandReviewSession(client: client)
            let review = try await session.prepare(ScheduleCommand.change(stored, change))
            let receipt = try await session.execute(review)
            #expect(receipt.schedule.announcement == stored.announcement)
            #expect(receipt.removed == (change == .remove))
            let requests = await transport.requests
            #expect(requests.allSatisfy { $0.url?.path.hasPrefix("/schedules") == true })
            #expect(await transport.writes().count == 1)
        }
    }

    @Test func recipeChangeAfterReviewPreventsMutation() async throws {
        let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
        await transport.setRows([savedAnnouncementFixture()])
        let stored = try #require(try await client.schedules().first)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(ScheduleCommand.change(stored, .remove))
        await transport.setRows([savedAnnouncementFixture(["text": .string("Changed"), "rooms": .array([])])])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}
