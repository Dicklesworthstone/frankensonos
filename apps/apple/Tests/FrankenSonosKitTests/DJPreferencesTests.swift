import Foundation
import Testing
@testable import FrankenSonosKit

private func preferenceJSON(energy: Any = NSNull(), explicit: Any = false) -> [String: Any] {
    let taste: [String: Any] = ["genres": [String](), "artists": [String](), "eras": [String](), "moods": [String]()]
    let items: [String: Any] = ["artists": [String](), "albums": [String](), "tracks": [String]()]
    return ["energy": energy, "explicit": explicit, "favor": taste, "avoid": taste, "pin": items, "ban": items]
}
private func decodePreferences(_ json: [String: Any]) throws -> DJPreferences {
    try JSONDecoder().decode(DJPreferences.self, from: JSONSerialization.data(withJSONObject: json))
}
private func stored(_ key: DJPreferenceKey, _ entries: [String], energy: Any = NSNull()) throws -> DJPreferences {
    var json = preferenceJSON(energy: energy)
    var group = json[key.group] as! [String: Any]; group[key.category] = entries; json[key.group] = group
    return try decodePreferences(json)
}
private func receipt(_ preferences: DJPreferences, changed: Bool = true, done: String = "Saved preference") throws -> DJPreferenceReceipt {
    let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(preferences))
    return try JSONDecoder().decode(DJPreferenceReceipt.self, from: JSONSerialization.data(withJSONObject: ["preferences": object, "changed": changed, "done": done]))
}

@Suite("Standing DJ preference contract") struct DJPreferencesContractTests {
    @Test func completeDefaultsAndEveryListAreReadWithoutInventingValues() throws {
        let base = try decodePreferences(preferenceJSON())
        #expect(base.energy == nil); #expect(!base.explicit)
        #expect(DJPreferenceKey.lists.count == 14)
        for key in DJPreferenceKey.lists {
            let prefs = try stored(key, ["An artist, with commas", "A whole recording"])
            #expect(prefs.entries(for: key) == ["An artist, with commas", "A whole recording"])
        }
    }
    @Test func missingRootFieldsNeverBecomeEmptyPreferences() throws {
        for key in ["energy", "explicit", "favor", "avoid", "pin", "ban"] {
            var json = preferenceJSON(); json.removeValue(forKey: key)
            #expect(throws: (any Error).self) { try decodePreferences(json) }
        }
    }
    @Test func everyNestedFieldMustBeAnArrayOfStrings() throws {
        for key in DJPreferenceKey.lists {
            for bad in [NSNull(), "jazz", [1], [true]] as [Any] {
                var json = preferenceJSON(), group = json[key.group] as! [String: Any]
                group[key.category] = bad; json[key.group] = group
                #expect(throws: (any Error).self) { try decodePreferences(json) }
            }
            var json = preferenceJSON(), group = json[key.group] as! [String: Any]
            group.removeValue(forKey: key.category); json[key.group] = group
            #expect(throws: (any Error).self) { try decodePreferences(json) }
        }
    }
    @Test func rejectsCoercedAndOutOfRangeEnergyAndExplicitSettings() {
        for value in [-1, 101, 2.5, "20", true] as [Any] {
            #expect(throws: (any Error).self) { try decodePreferences(preferenceJSON(energy: value)) }
        }
        for value in [0, "false", NSNull()] as [Any] {
            #expect(throws: (any Error).self) { try decodePreferences(preferenceJSON(explicit: value)) }
        }
    }
    @Test func listBoundsCountUnicodeScalarsLikeTheBackendNotGraphemeClusters() throws {
        let combined = String(repeating: "e\u{301}", count: 128)
        #expect(combined.count == 128)
        #expect(try stored(.favorArtists, [combined]).favor.artists == [combined])
        #expect(throws: (any Error).self) { try stored(.favorArtists, [combined + "x"]) }
        #expect(throws: (any Error).self) { try stored(.favorArtists, ["   "]) }
        #expect(throws: (any Error).self) { try stored(.favorArtists, Array(repeating: "x", count: 5_001)) }
    }
    @Test func namesURIsDuplicatesAndUnknownFutureFieldsAreNotReinterpreted() throws {
        var json = preferenceJSON(); json["future_constraint"] = ["value": 42]
        let base = try decodePreferences(json)
        #expect(base.energy == nil)
        let names = ["A & B, C", "A & B, C", "spotify:track:fixture", "<literal title>"]
        #expect(try stored(.pinTracks, names).pin.tracks == names)
        let bytes = try JSONEncoder().encode(DJPreferenceChange(key: .favorGenres, value: "jazz"))
        let fields = try #require(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
        #expect(Set(fields.keys) == ["key", "value", "unset"])
        #expect(fields["zone"] == nil && fields["preferences"] == nil && fields["future_constraint"] == nil)
    }
    @Test func eachListSupportsOneLiteralAdditionOrNamedRemoval() throws {
        for key in DJPreferenceKey.lists {
            let change = try DJPreferenceChange(key: key, value: "  One, not two  ")
            #expect(change.value == "One, not two" && !change.unset)
            let remove = try DJPreferenceChange(key: key, value: "One, not two", unset: true)
            let body = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(remove)) as? [String: Any])
            #expect(body["key"] as? String == key.rawValue)
            #expect(body["value"] as? String == "One, not two")
            #expect(body["unset"] as? Bool == true)
        }
    }
    @Test func invalidListInputCannotTurnIntoAClearAll() {
        for value in [nil, "", " \n", "bad\nname", String(repeating: "x", count: 257)] as [String?] {
            for unset in [false, true] {
                #expect(throws: (any Error).self) { try DJPreferenceChange(key: .banArtists, value: value, unset: unset) }
            }
        }
    }
    @Test func energyIsCanonicalAndResetOmitsValue() throws {
        #expect(try DJPreferenceChange(key: .energy, value: " 00025 ").value == "25")
        for value in ["-1", "+1", "1.5", "1e1", "101", "true", "１", ""] {
            #expect(throws: (any Error).self) { try DJPreferenceChange(key: .energy, value: value) }
        }
        let reset = try DJPreferenceChange(key: .energy, unset: true)
        let body = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(reset)) as? [String: Any])
        #expect(body["value"] == nil && body["unset"] as? Bool == true)
        #expect(reset.title == "Follow time-of-day energy")
    }
    @Test func explicitContentNeverCoercesTextToABoolean() throws {
        for value in ["TRUE", "1", "yes", "allowed", ""] {
            #expect(throws: (any Error).self) { try DJPreferenceChange(key: .explicit, value: value) }
        }
        #expect(try DJPreferenceChange(key: .explicit, value: "false").title == "Keep explicit tracks out")
        #expect(try DJPreferenceChange(key: .explicit, unset: true).value == nil)
    }
    @Test func removalRequiresTheExactObservedEntryAndBaseline() throws {
        let base = try stored(.banArtists, ["A", "B"])
        let remove = try DJPreferenceChange(key: .banArtists, value: "A", unset: true)
        let command = try DJPreferenceCommand(change: remove, before: base)
        try command.validate(base)
        #expect(throws: (any Error).self) { try DJPreferenceCommand(change: remove, before: stored(.banArtists, ["a"])) }
        #expect(throws: (any Error).self) { try command.validate(stored(.banArtists, ["A", "C"])) }
    }
    @Test func evenAnUnrelatedPreferenceEditRequiresANewReview() throws {
        let base = try decodePreferences(preferenceJSON())
        let command = try DJPreferenceCommand(change: DJPreferenceChange(key: .energy, value: "30"), before: base)
        #expect(throws: (any Error).self) { try command.validate(decodePreferences(preferenceJSON(explicit: true))) }
    }
    @Test func receiptsConfirmEnergyExplicitAddAndRemove() throws {
        let base = try decodePreferences(preferenceJSON())
        for (key, value, after) in [
            (DJPreferenceKey.energy, "0", try decodePreferences(preferenceJSON(energy: 0))),
            (.explicit, "true", try decodePreferences(preferenceJSON(explicit: true))),
            (.favorGenres, "jazz", try stored(.favorGenres, ["jazz"]))
        ] {
            let command = try DJPreferenceCommand(change: DJPreferenceChange(key: key, value: value), before: base)
            #expect(try receipt(after).validated(for: command).preferences == after)
        }
        let before = try stored(.banArtists, ["A", "B"])
        let command = try DJPreferenceCommand(change: DJPreferenceChange(key: .banArtists, value: "A", unset: true), before: before)
        #expect(try receipt(stored(.banArtists, ["B"])).validated(for: command).changed)
    }
    @Test func scalarResetUsesActualBackendDefaults() throws {
        let base = try decodePreferences(preferenceJSON(energy: 40, explicit: true))
        let energy = try DJPreferenceCommand(change: DJPreferenceChange(key: .energy, unset: true), before: base)
        _ = try receipt(decodePreferences(preferenceJSON(explicit: true))).validated(for: energy)
        let explicit = try DJPreferenceCommand(change: DJPreferenceChange(key: .explicit, unset: true), before: base)
        _ = try receipt(decodePreferences(preferenceJSON(energy: 40))).validated(for: explicit)
    }
    @Test func normalizedNoOpDoesNotInventASecondListEntry() throws {
        let base = try stored(.favorGenres, ["Jazz"])
        let command = try DJPreferenceCommand(change: DJPreferenceChange(key: .favorGenres, value: "jazz"), before: base)
        let report = try receipt(base, changed: false).validated(for: command)
        #expect(!report.changed && report.preferences.favor.genres == ["Jazz"])
    }
    @Test func contradictoryAndIncompleteAcknowledgementsAreNotSuccess() throws {
        let base = try decodePreferences(preferenceJSON())
        let command = try DJPreferenceCommand(change: DJPreferenceChange(key: .energy, value: "30"), before: base)
        #expect(throws: (any Error).self) { try receipt(base).validated(for: command) }
        #expect(throws: (any Error).self) { try receipt(decodePreferences(preferenceJSON(energy: 30)), changed: false).validated(for: command) }
        #expect(throws: (any Error).self) { try receipt(decodePreferences(preferenceJSON(energy: 30)), done: " ").validated(for: command) }
        for text in ["null", "{}", "{\"done\":\"ok\",\"changed\":true}"] {
            #expect(throws: (any Error).self) { try JSONDecoder().decode(DJPreferenceReceipt.self, from: Data(text.utf8)) }
        }
    }
    @Test func retainedRemovedEntryCannotBeAcknowledgedAsRemoved() throws {
        let before = try stored(.banTracks, ["A"])
        let command = try DJPreferenceCommand(change: DJPreferenceChange(key: .banTracks, value: "A", unset: true), before: before)
        #expect(throws: (any Error).self) { try receipt(before).validated(for: command) }
    }
}

private actor PreferenceReader {
    var calls = 0
    var waiters: [CheckedContinuation<DJPreferences, any Error>] = []
    func read() async throws -> DJPreferences {
        calls += 1
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }
    func finish(_ prefs: DJPreferences) { waiters.removeFirst().resume(returning: prefs) }
    func fail() { waiters.removeFirst().resume(throwing: DJPreferencesError.invalid("The daemon is unavailable.")) }
}
@MainActor private func eventually(_ condition: () async -> Bool) async throws {
    for _ in 0..<300 { if await condition() { return }; try await Task.sleep(for: .milliseconds(2)) }
    Issue.record("Condition did not become true")
}

@Suite("Standing DJ preferences read lifecycle") @MainActor struct DJPreferencesLifecycleTests {
    @Test func staysLazyAndAQuietOpenEditorNeverPolls() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        #expect(await reader.calls == 0); model.resume(); model.resume()
        try await eventually { await reader.calls == 1 }; await reader.finish(base)
        try await eventually { model.canEdit }
        try await Task.sleep(for: .milliseconds(20))
        #expect(await reader.calls == 1); model.stop()
    }
    @Test func failedRefreshPreservesTheOriginalObservationTimeButNotAuthority() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        model.resume(); try await eventually { await reader.calls == 1 }; await reader.finish(base)
        try await eventually { model.canEdit }; let observed = try #require(model.observation)
        model.refresh(); try await eventually { await reader.calls == 2 }; await reader.fail()
        try await eventually { !model.loading }
        #expect(model.observation?.id == observed.id && model.observation?.observedAt == observed.observedAt)
        #expect(!model.canEdit && model.problem != nil)
        #expect(throws: (any Error).self) { try model.command(DJPreferenceChange(key: .energy, value: "30"), from: observed) }
        model.stop()
    }
    @Test func refreshRetiresEvenAnIdenticalEarlierSelection() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        model.resume(); try await eventually { await reader.calls == 1 }; await reader.finish(base)
        try await eventually { model.canEdit }; let old = try #require(model.observation)
        model.refresh(); try await eventually { await reader.calls == 2 }; await reader.finish(base)
        try await eventually { model.canEdit }
        let change = try DJPreferenceChange(key: .energy, value: "30")
        #expect(throws: (any Error).self) { try model.command(change, from: old) }
        _ = try model.command(change, from: #require(model.observation)); model.stop()
    }
    @Test func rapidRefreshesCoalesceAndNeverOverlapIgnoredCancellation() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        model.resume(); try await eventually { await reader.calls == 1 }
        for _ in 0..<50 { model.refresh() }
        #expect(await reader.calls == 1)
        await reader.finish(base); try await eventually { await reader.calls == 2 }
        #expect(model.observation == nil && !model.canEdit)
        await reader.finish(base); try await eventually { model.canEdit }
        #expect(await reader.calls == 2); model.stop()
    }
    @Test func suspendAndImmediateResumeDrainOldReadBeforePublishingNewState() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        model.resume(); try await eventually { await reader.calls == 1 }
        model.suspend(); model.resume(); #expect(await reader.calls == 1)
        await reader.finish(try decodePreferences(preferenceJSON(energy: 10)))
        try await eventually { await reader.calls == 2 }
        #expect(model.observation == nil)
        await reader.finish(base); try await eventually { model.canEdit }
        #expect(model.observation?.preferences.energy == nil); model.stop()
    }
    @Test func stoppedEditorCannotRestartOrAcceptALateReply() async throws {
        let reader = PreferenceReader(), base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { try await reader.read() })
        model.resume(); try await eventually { await reader.calls == 1 }; model.stop()
        model.resume(); model.refresh(); await reader.finish(base)
        try await eventually { !model.loading }
        #expect(!model.active && model.observation == nil && !model.canEdit)
        #expect(await reader.calls == 1)
    }
    @Test func anEmptyCompleteDocumentIsAnObservationNotAReadFailure() async throws {
        let base = try decodePreferences(preferenceJSON())
        let model = DJPreferencesModel(read: { base }); model.resume()
        try await eventually { model.canEdit }
        #expect(model.problem == nil && model.observation?.preferences == base)
        model.stop()
    }
    @Test func firstReadFailureDoesNotCreateDefaults() async throws {
        let model = DJPreferencesModel(read: { throw DJPreferencesError.invalid("Unavailable") })
        model.resume(); try await eventually { !model.loading }
        #expect(model.observation == nil && !model.fresh && model.problem == "Unavailable")
        model.stop()
    }
}
