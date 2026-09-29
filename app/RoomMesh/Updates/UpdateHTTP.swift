import Foundation

/// The updater's network access, injectable so tests use fixtures.
protocol UpdateHTTP: Sendable {
    /// GET `url`; returns the body and HTTP status (non-2xx isn't thrown).
    func get(_ url: URL, headers: [String: String]) async throws -> (Data, Int)
    /// Downloads `url` to `destination` (replacing it), reporting progress 0…1 from any thread.
    /// Throws on a transport error or a non-2xx status.
    func download(_ url: URL, headers: [String: String], to destination: URL,
                  progress: @escaping @Sendable (Double) -> Void) async throws
}

struct URLSessionUpdateHTTP: UpdateHTTP {
    let session: URLSession

    init(session: URLSession = URLSession(configuration: .ephemeral)) { self.session = session }

    private func request(_ url: URL, _ headers: [String: String], timeout: TimeInterval) -> URLRequest {
        var r = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: timeout)
        for (k, v) in headers { r.setValue(v, forHTTPHeaderField: k) }
        return r
    }

    func get(_ url: URL, headers: [String: String]) async throws -> (Data, Int) {
        let (data, response) = try await session.data(for: request(url, headers, timeout: 30))
        return (data, (response as? HTTPURLResponse)?.statusCode ?? 0)
    }

    func download(_ url: URL, headers: [String: String], to destination: URL,
                  progress: @escaping @Sendable (Double) -> Void) async throws {
        let req = request(url, headers, timeout: 60)
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            final class Box: @unchecked Sendable { var observation: NSKeyValueObservation? }
            let box = Box()
            let task = session.downloadTask(with: req) { tmp, response, error in
                defer { box.observation = nil }
                if let error { return cont.resume(throwing: error) }
                guard let tmp, let http = response as? HTTPURLResponse else { return cont.resume(throwing: URLError(.badServerResponse)) }
                guard (200..<300).contains(http.statusCode) else { return cont.resume(throwing: UpdateError.http(http.statusCode)) }
                // The temporary file is deleted when this handler returns: move it now.
                do {
                    try? FileManager.default.removeItem(at: destination)
                    try FileManager.default.moveItem(at: tmp, to: destination)
                    cont.resume()
                } catch {
                    cont.resume(throwing: error)
                }
            }
            box.observation = task.progress.observe(\.fractionCompleted) { p, _ in progress(p.fractionCompleted) }
            task.resume()
        }
    }
}
