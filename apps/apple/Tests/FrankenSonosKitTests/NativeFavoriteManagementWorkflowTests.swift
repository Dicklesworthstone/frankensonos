import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

@MainActor private func favoriteManagedWorkflow(_ transport: NativeFavoriteManagementTransport) throws -> FavoriteManagementWorkflow {
    let client = try favoriteManagedClient(transport)
    let workflow = FavoriteManagementWorkflow(client: client, commands: CommandReviewSession(client: client))
    workflow.resume(); return workflow
}

@Suite("Native favorite workflow and remote integration") struct NativeFavoriteManagementWorkflowTests {
    @Test @MainActor func nativeModelUsesSharedLaneAndSelectionRetiresUnsentReview() async throws {
        let transport = NativeFavoriteManagementTransport(), model = try RemoteModel(client: favoriteManagedClient(transport))
        model.resume(); model.show(.favorites)
        defer { model.stop() }
        try await favoriteManagedWait { await MainActor.run {
            model.canPrepare && !model.panelLoading && model.favorites.first != nil
        } }
        model.openFavoriteSave(in: favoriteManagedZone())
        let workflow = model.favoriteManagement
        workflow.editTitle("Keep"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        #expect(!model.canPrepare && !model.canSchedule)
        model.prepare(.pause); model.openPlaylistSave()
        #expect(model.review == nil && !model.playlistManagement.isOpen)
        model.select(RoomID(household: "S2", room: "Room + &"))
        try await favoriteManagedWait { await MainActor.run { !workflow.isEngaged } }
        #expect(workflow.review == nil && !workflow.isOpen)
        workflow.confirm(review.id)
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func modelRejectsStaleRenderedHouseholdAndFavoriteIdentity() async throws {
        let transport = NativeFavoriteManagementTransport(), model = try RemoteModel(client: favoriteManagedClient(transport))
        model.resume(); model.show(.favorites)
        defer { model.stop() }
        try await favoriteManagedWait { await MainActor.run {
            model.canPrepare && !model.panelLoading && model.favorites.first != nil
        } }
        model.openFavoriteSave(in: favoriteManagedZone("S2"))
        #expect(!model.favoriteManagement.isOpen)
        model.openFavoriteManagement(favoriteManagedItem(), in: favoriteManagedZone("S2"), removing: false)
        #expect(!model.favoriteManagement.isOpen)
        model.openFavoriteManagement(favoriteManagedItem("Stale title"), in: favoriteManagedZone(), removing: true)
        #expect(!model.favoriteManagement.isOpen)
        model.openFavoriteManagement(favoriteManagedItem(), in: favoriteManagedZone(), removing: false)
        #expect(model.favoriteManagement.isOpen && model.favoriteManagement.favorite?.id == "FV:2/7")
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func uncertainReceiptSurvivesSelectionAndRecoversTheOriginalRegroupedHousehold() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        #expect(workflow.openSave(in: favoriteManagedZone()))
        workflow.editTitle("Evening"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(workflow.result?.kind == .uncertain && workflow.needsRefresh)
        workflow.close()
        #expect(workflow.openSave(in: favoriteManagedZone("S2")))
        #expect(workflow.operation == nil && workflow.result?.command.zone.household == "S1")
        await transport.regroup([favoriteManagedZone(coordinator: "Hall"), favoriteManagedZone("S2")])
        workflow.checkCatalog()
        try await favoriteManagedWait { await MainActor.run { workflow.recovery != nil && !workflow.isEngaged } }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        #expect(workflow.recovery?.target == favoriteManagedZone().id)
        #expect(workflow.recovery?.catalog.map(\.id) == ["FV:2/7", "FV:2/8"])
        #expect(workflow.recovery?.detail.contains("does not prove") == true)
        let last = try #require(await transport.requests.last(where: { $0.url?.path == "/favorites" }))
        #expect(URLComponents(url: last.url!, resolvingAgainstBaseURL: false)?.queryItems == [URLQueryItem(name: "zone", value: "Hall@S1")])
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func failedOrCancelledRecoveryCannotClearUncertaintyOrLoseReceipt() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openRemove(favoriteManagedItem(), in: favoriteManagedZone()); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        let resultID = workflow.result?.id
        await transport.fail("/favorites"); workflow.checkCatalog()
        try await favoriteManagedWait { await MainActor.run { workflow.problem != nil && !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.result?.id == resultID)
        workflow.dismissResult(); #expect(workflow.result?.id == resultID)
        await transport.fail(nil); await transport.hold("GET", "/favorites")
        workflow.checkCatalog(); try await favoriteManagedWait { await transport.blocked }
        workflow.close(); await transport.release()
        try await favoriteManagedWait { await MainActor.run { !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.recovery == nil)
        workflow.openResult(); workflow.checkCatalog()
        try await favoriteManagedWait { await MainActor.run { workflow.recovery != nil && !workflow.isEngaged } }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        #expect(workflow.recovery?.detail.contains("not currently listed") == true)
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func absentOriginalRoomCannotSubstituteTheSameNameInAnotherHousehold() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openRename(favoriteManagedItem(), in: favoriteManagedZone()); workflow.editTitle("After"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        await transport.regroup([favoriteManagedZone("S2")])
        let reads = await transport.requests.count
        workflow.checkCatalog()
        try await favoriteManagedWait { await MainActor.run { workflow.problem != nil && !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.recovery == nil)
        let later = Array(await transport.requests.dropFirst(reads))
        #expect(later.allSatisfy { $0.url?.path == "/zones" })
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func lateConfirmedReceiptKeepsItsOriginalTargetWhileSuspended() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openSave(in: favoriteManagedZone()); workflow.editTitle("Keep"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.hold("POST", "/favorites/save"); workflow.confirm(review.id)
        try await favoriteManagedWait { await transport.blocked }
        workflow.suspend(); #expect(workflow.isEngaged)
        await transport.release()
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(!workflow.active && workflow.result?.kind == .confirmed)
        #expect(workflow.result?.command.zone.household == "S1" && workflow.result?.receipt?.favorite.id == "FV:2/8")
        #expect(await transport.writes().count == 1)
        workflow.resume(); #expect(workflow.openResult())
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func closeDuringPreparationCannotPublishALateReview() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openSave(in: favoriteManagedZone()); workflow.editTitle("Keep")
        await transport.hold("GET", "/favorites/current"); workflow.prepare()
        try await favoriteManagedWait { await transport.blocked }
        workflow.close(); await transport.release()
        try await favoriteManagedWait { await MainActor.run { !workflow.isEngaged } }
        #expect(!workflow.isOpen && workflow.review == nil && workflow.result == nil)
        #expect(workflow.openSave(in: favoriteManagedZone("S2")))
        #expect(workflow.review == nil)
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func changedSourceBeforeConfirmationReportsNoWrite() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openSave(in: favoriteManagedZone()); workflow.editTitle("Keep"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.changeSource(revision: "different-song"); workflow.confirm(review.id)
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(workflow.result?.kind == .notSent && !workflow.needsRefresh)
        #expect(workflow.result?.receipt == nil && workflow.review == nil)
        workflow.confirm(review.id)
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func catalogInvalidationCannotReleaseOrReplayUncertainRecovery() async throws {
        let transport = NativeFavoriteManagementTransport(), workflow = try favoriteManagedWorkflow(transport)
        defer { workflow.stop() }
        workflow.openRemove(favoriteManagedItem(), in: favoriteManagedZone()); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let review = try #require(workflow.review)
        await transport.loseReply(); workflow.confirm(review.id)
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        let result = try #require(workflow.result)
        await transport.hold("GET", "/favorites")
        workflow.checkCatalog(); try await favoriteManagedWait { await transport.blocked }
        let before = NativeCatalogRevisions()
        var changed = before
        changed.invalidate(household: "S1", favorites: true, playlists: false,
                           zones: [favoriteManagedZone(), favoriteManagedZone("S2")])
        workflow.observeCatalogs(changed, since: before)
        await transport.release()
        try await favoriteManagedWait { await MainActor.run { !workflow.isEngaged } }
        #expect(workflow.needsRefresh && workflow.recovery == nil && workflow.result?.id == result.id)
        workflow.checkCatalog()
        try await favoriteManagedWait { await MainActor.run { !workflow.isEngaged && workflow.recovery != nil } }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        var other = changed
        other.invalidate(household: "S2", favorites: true, playlists: false,
                         zones: [favoriteManagedZone(), favoriteManagedZone("S2")])
        workflow.observeCatalogs(other, since: changed)
        #expect(!workflow.needsRefresh && workflow.recovery != nil)
        changed = other
        changed.invalidate(household: "S1", favorites: false, playlists: true,
                           zones: [favoriteManagedZone(), favoriteManagedZone("S2")])
        workflow.observeCatalogs(changed, since: other)
        #expect(!workflow.needsRefresh && workflow.recovery != nil)
        other = changed
        other.invalidate(household: "S1", favorites: true, playlists: false,
                         zones: [favoriteManagedZone(), favoriteManagedZone("S2")])
        workflow.observeCatalogs(other, since: changed)
        #expect(workflow.needsRefresh && workflow.recovery == nil && workflow.result?.kind == .uncertain)
        #expect(await transport.writes().count == 1)
    }

    @Test @MainActor func nativeFavoriteEventsFenceLateCatalogReads() async throws {
        let transport = NativeFavoriteManagementTransport(), model = try RemoteModel(client: favoriteManagedClient(transport))
        model.resume(); model.show(.favorites)
        defer { model.stop() }
        try await favoriteManagedWait { await MainActor.run {
            model.favorites.first?.title == "Evening" && !model.panelLoading && (model.house?.catalogReadRevision ?? 0) > 0
        } }
        let before = try #require(model.house)
        await transport.hold("GET", "/favorites"); model.refreshPanel()
        try await favoriteManagedWait { await transport.blocked }
        await transport.replace([favoriteManagedItem("Changed elsewhere")])
        await transport.catalogEvent(count: 100)
        try await favoriteManagedWait { await MainActor.run {
            (model.house?.catalogReadRevision ?? 0) > before.catalogReadRevision && model.favorites.isEmpty
        } }
        await transport.release()
        try await favoriteManagedWait { await MainActor.run {
            !model.panelLoading && model.favorites.first?.title == "Changed elsewhere"
        } }
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func nativeFavoriteEventsRetireUnsentReviewsAndPreserveSentReceipts() async throws {
        let transport = NativeFavoriteManagementTransport(), model = try RemoteModel(client: favoriteManagedClient(transport))
        model.resume(); model.show(.favorites)
        defer { model.stop() }
        try await favoriteManagedWait { await MainActor.run {
            model.favorites.first != nil && !model.panelLoading && (model.house?.catalogReadRevision ?? 0) > 0
        } }
        let workflow = model.favoriteManagement
        model.openFavoriteManagement(favoriteManagedItem(), in: favoriteManagedZone(), removing: false)
        workflow.editTitle("Updated"); workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let original = try #require(workflow.review), before = try #require(model.house)
        await transport.catalogEvent("S2")
        try await favoriteManagedWait { await MainActor.run {
            (model.house?.catalogReadRevision ?? 0) > before.catalogReadRevision
        } }
        #expect(workflow.review?.id == original.id)
        await transport.catalogEvent()
        try await favoriteManagedWait { await MainActor.run { workflow.review == nil && !workflow.isEngaged } }
        workflow.confirm(original.id)
        #expect(await transport.writes().isEmpty)
        workflow.prepare()
        try await favoriteManagedWait { await MainActor.run { workflow.review != nil && workflow.phase == .idle } }
        let current = try #require(workflow.review)
        await transport.hold("POST", "/favorites/rename"); workflow.confirm(current.id)
        try await favoriteManagedWait { await transport.blocked }
        let sent = try #require(model.house)
        await transport.catalogEvent()
        try await favoriteManagedWait { await MainActor.run {
            (model.house?.catalogReadRevision ?? 0) > sent.catalogReadRevision
        } }
        #expect(workflow.phase == .confirming && workflow.result == nil)
        await transport.release()
        try await favoriteManagedWait { await MainActor.run { workflow.result != nil && !workflow.isEngaged } }
        #expect(workflow.result?.kind == .confirmed && workflow.result?.receipt?.favorite.title == "Updated")
        #expect(await transport.writes().count == 1)
    }
}
