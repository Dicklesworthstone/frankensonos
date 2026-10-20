import Foundation

/// Recovery is scoped to the daemon's authenticated caller, not the selected
/// room or an arbitrary action-log row. Only the preview endpoint chooses it.
public struct UndoReview: Identifiable, Sendable {
    public let id = UUID()
    public let action: LoggedAction
    public let observedAt: Date
}

/// A checked undo acknowledges the exact original action. A successful HTTP
/// reply can still describe a partial restore; retain every reported failure.
public struct UndoReceipt: Decodable, Equatable, Sendable {
    public let summary: String
    public let undone: Int64
    public let intent: String
    public let failures: [String]

    private enum CodingKeys: String, CodingKey { case summary, undone, intent, failures }

    public init(from decoder: any Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        summary = try values.decode(String.self, forKey: .summary)
        undone = try values.decode(Int64.self, forKey: .undone)
        intent = try values.decode(String.self, forKey: .intent)
        failures = values.contains(.failures) ? try values.decode([String].self, forKey: .failures) : []
    }

    func validated(for action: LoggedAction) throws -> Self {
        func validText(_ value: String) -> Bool {
            !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 65_536
        }
        guard undone == action.id, intent == action.intent, validText(summary), validText(intent),
              failures.count <= 2_000, failures.allSatisfy(validText) else {
            throw ContractError.invalid("The undo reply does not acknowledge the reviewed action completely.")
        }
        return self
    }
}

public struct RemoteUndoResult: Identifiable, Sendable {
    public let id = UUID()
    public let action: LoggedAction?
    public let receipt: UndoReceipt?
    public let title: String
    public let detail: String
    public let kind: RemoteResult.Kind
}

extension LoggedAction {
    func validateUndoCandidate() throws {
        try validate()
        guard undoable, undoOf == nil,
              !client.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !intent.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw ContractError.invalid("The daemon did not return a valid action to undo. Read the available undo again.")
        }
    }
}

extension DaemonClient {
    public func undoPreview() async throws -> LoggedAction? {
        try Task.checkCancellation()
        let action = try await get(LoggedAction?.self, ["undo", "preview"])
        try action?.validateUndoCandidate()
        try Task.checkCancellation()
        return action
    }
}
