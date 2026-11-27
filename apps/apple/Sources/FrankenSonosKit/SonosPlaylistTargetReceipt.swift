import Foundation

private func checkedGroup(_ audience: SonosPlaylistAudience, household: String,
                          coordinator: String, zone: String, seen: inout Set<String>) throws {
    guard let group = audience.groups.first(where: {
        $0.household == household && $0.coordinator == coordinator
    }), PlaylistContentsRequest.exact(group.coordinatorRoom, zone),
          seen.insert(group.id).inserted else { throw ClientError.unreadable }
}

private func checkedNotes(_ notes: [Note]) throws {
    guard notes.allSatisfy({ SonosPlaylistAudience.validText($0.code) && !$0.detail.isEmpty }) else {
        throw ClientError.unreadable
    }
}

struct SonosPlaylistBatchSummary {
    let detail: String
    let partial: Bool
}

/// Decode each target, including skipped and unattempted groups. A summary flag
/// cannot turn an individual failure into a complete result.
struct SonosPlaylistPlaybackBatch: Decodable {
    struct Target: Decodable {
        struct Result: Decodable {
            let status: String
            let done: String?
            let changed: Bool?
            let notes: [Note]?
            let error: APIProblem?
        }
        let household: String
        let target: String
        let zone: String
        let result: Result
    }
    let complete: Bool
    let results: [Target]

    func summary(for audience: SonosPlaylistAudience, changed: Bool) throws -> SonosPlaylistBatchSummary {
        guard results.count == audience.groups.count else { throw ClientError.unreadable }
        var seen = Set<String>(), lines: [String] = []
        var partial = !complete
        for row in results {
            try checkedGroup(audience, household: row.household, coordinator: row.target, zone: row.zone, seen: &seen)
            let label = "\(row.zone) · \(row.household)"
            switch row.result.status {
            case "succeeded":
                guard let done = row.result.done, !done.isEmpty, let didChange = row.result.changed,
                      !didChange || changed, row.result.error == nil else { throw ClientError.unreadable }
                let notes = row.result.notes ?? []
                try checkedNotes(notes)
                partial = partial || notes.contains { $0.code == "SKIPPED" }
                lines.append("\(label): \(done)")
                lines.append(contentsOf: notes.map(\.detail))
            case "failed":
                guard let error = row.result.error, SonosPlaylistAudience.validText(error.code),
                      !error.detail.isEmpty else { throw ClientError.unreadable }
                partial = true
                lines.append("\(label): \(error.code == "OUTCOME_UNCERTAIN" ? "Outcome uncertain" : "Failed") · \(error.detail)")
                if let hint = error.hint, !hint.isEmpty { lines.append(hint) }
            case "not_attempted":
                partial = true; lines.append("\(label): Not attempted after an earlier failure.")
            default: throw ClientError.unreadable
            }
        }
        if partial { lines.append("Inspect every affected group before another request. This request was not repeated.") }
        return SonosPlaylistBatchSummary(detail: lines.joined(separator: "\n"), partial: partial)
    }
}

/// Successful insertion ranges and definite/uncertain failures stay attributed
/// to the reviewed physical audience. No missing row is inferred successful.
struct SonosPlaylistQueueBatch: Decodable {
    struct Added: Decodable {
        struct Queue: Decodable {
            let done: String
            let zone: String
            let first: UInt32
            let count: UInt32
            let playingQueue: Bool
            let notes: [Note]?
        }
        let household: String
        let coordinator: String
        let queue: Queue
    }
    struct Failed: Decodable {
        let household: String
        let coordinator: String
        let zone: String
        let error: APIProblem
    }
    let done: String
    let complete: Bool
    let added: [Added]
    let failed: [Failed]
    let logged: Bool
    let undoable: Bool

    var single: Added.Queue? { added.count == 1 && failed.isEmpty ? added.first?.queue : nil }

    func summary(for audience: SonosPlaylistAudience, count: UInt32, changed: Bool) throws -> SonosPlaylistBatchSummary {
        guard !done.isEmpty, added.count + failed.count == audience.groups.count else { throw ClientError.unreadable }
        var seen = Set<String>(), lines: [String] = []
        let uncertain = failed.contains { $0.error.code == "OUTCOME_UNCERTAIN" }
        guard changed || (added.isEmpty && !uncertain), !undoable || (logged && !uncertain) else {
            throw ClientError.unreadable
        }
        for row in added {
            try checkedGroup(audience, household: row.household, coordinator: row.coordinator, zone: row.queue.zone, seen: &seen)
            guard !row.queue.done.isEmpty, row.queue.first > 0, row.queue.count == count,
                  UInt64(row.queue.first) + UInt64(count) - 1 <= UInt64(UInt32.max) else { throw ClientError.unreadable }
            try checkedNotes(row.queue.notes ?? [])
            lines.append("\(row.queue.zone) · \(row.household): Added \(count) selected occurrences at position \(row.queue.first). Playback was not started or restarted.")
            lines.append(contentsOf: (row.queue.notes ?? []).map(\.detail))
        }
        for row in failed {
            try checkedGroup(audience, household: row.household, coordinator: row.coordinator, zone: row.zone, seen: &seen)
            guard SonosPlaylistAudience.validText(row.error.code), !row.error.detail.isEmpty else { throw ClientError.unreadable }
            lines.append("\(row.zone) · \(row.household): \(row.error.code == "OUTCOME_UNCERTAIN" ? "Outcome uncertain" : "Failed") · \(row.error.detail)")
            if let hint = row.error.hint, !hint.isEmpty { lines.append(hint) }
        }
        if !logged { lines.append("The action was not logged. Repeating it cannot repair the log.") }
        lines.append(undoable ? "The daemon retained verified range undo; review Activity before restoring."
            : "No aggregate undo was offered for this result.")
        let partial = !complete || !failed.isEmpty || added.isEmpty
        if partial { lines.append("Inspect every affected group before another request. This request was not repeated.") }
        return SonosPlaylistBatchSummary(detail: lines.joined(separator: "\n"), partial: partial)
    }
}
