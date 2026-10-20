import Foundation

extension AnnouncementWav {
    /// Read only the locally selected regular file, off the UI actor. No URL,
    /// bookmark or filename is retained by the request; only validated bytes.
    public static func load(from url: URL) async throws -> Self {
        try Task.checkCancellation()
        let worker = Task.detached(priority: .userInitiated) { try Self.readFile(url) }
        return try await withTaskCancellationHandler {
            let clip = try await worker.value
            try Task.checkCancellation()
            return clip
        } onCancel: { worker.cancel() }
    }

    private static func readFile(_ url: URL) throws -> Self {
        try Task.checkCancellation()
        guard url.isFileURL else {
            throw ContractError.invalid("Choose a local WAV file, not a network address.")
        }
        #if canImport(Darwin)
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        #endif
        do {
            let info = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
            guard info.isRegularFile == true else {
                throw ContractError.invalid("Choose a regular WAV file, not a folder or device.")
            }
            guard (info.fileSize ?? 0) <= Self.maximumBytes else {
                throw ContractError.invalid("The WAV exceeds the 16 MiB announcement limit.")
            }
            try Task.checkCancellation()
            let file = try FileHandle(forReadingFrom: url)
            defer { try? file.close() }
            var data = Data()
            // The file can grow after its metadata was read. Never buffer
            // more than the limit plus one byte, regardless of that metadata.
            while data.count <= Self.maximumBytes {
                try Task.checkCancellation()
                let count = min(64 * 1_024, Self.maximumBytes + 1 - data.count)
                guard let chunk = try file.read(upToCount: count), !chunk.isEmpty else { break }
                data.append(chunk)
            }
            try Task.checkCancellation()
            return try Self(data: data, name: url.lastPathComponent)
        } catch is CancellationError {
            throw CancellationError()
        } catch let error as ContractError {
            throw error
        } catch {
            // File-provider errors may contain private local paths. The user
            // needs a remedy, not the path echoed into an action result.
            throw ContractError.invalid("The selected WAV could not be read. Make it available on this device and choose it again. Nothing was uploaded.")
        }
    }
}
