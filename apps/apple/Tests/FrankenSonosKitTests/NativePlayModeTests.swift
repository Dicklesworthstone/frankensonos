import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

/// A scene-store operation and speaker settings must consume the same native
/// review, even though one targets the whole saved house and one a coordinator.
@Suite("Shared scene and listening-settings command gate") struct NativeSettingsSceneGateTests {
    private var group: Zone {
        Zone(household: "TEST-A", coordinatorRoom: "Studio", members: ["Studio", "Kitchen"], transportState: "paused")
    }
    private func settingsCommand() throws -> NativeCommand {
        .listeningSettings(.order(try PlaybackOrderChange(group: group,
            before: QueuePlaybackOrder(zone: "Studio", mode: .normal, queueActive: true, djActive: false),
            desired: .shuffleNoRepeat)))
    }
    private func session(_ transport: SettingsIntegrationTransport) throws -> CommandReviewSession {
        CommandReviewSession(client: DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport))
    }

    @Test func listeningReviewRetiresAnUnsentSceneCaptureWithoutSavingIt() async throws {
        let transport = SettingsIntegrationTransport(), session = try self.session(transport)
        let oldScene = try await session.prepare(SceneMutation.capture(name: "Fresh scene", replacing: nil))
        let reviewedSetting = try await session.prepare(settingsCommand(), in: group)
        await #expect(throws: ContractError.self) { try await session.execute(oldScene) }
        _ = try await session.execute(reviewedSetting)
        #expect(await transport.mutations().map { $0.url?.path } == ["/play-mode"])
        #expect(await transport.scenes.isEmpty)
        #expect(await transport.applied == 1)
    }

    @Test func sceneReviewRetiresAnUnsentSettingWithoutChangingPlaybackOrder() async throws {
        let transport = SettingsIntegrationTransport(), session = try self.session(transport)
        let oldSetting = try await session.prepare(settingsCommand(), in: group)
        let reviewedScene = try await session.prepare(SceneMutation.capture(name: "Fresh scene", replacing: nil))
        await #expect(throws: ContractError.self) { try await session.execute(oldSetting) }
        let receipt = try await session.execute(reviewedScene)
        #expect(receipt.saved?.name == "Fresh scene")
        #expect(await transport.mutations().map { $0.httpMethod } == ["PUT"])
        #expect(await transport.modes.isEmpty)
        #expect(await transport.applied == 1)
    }
}
