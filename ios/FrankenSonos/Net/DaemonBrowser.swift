import Foundation
import Network

struct DiscoveredDaemon: Identifiable, Equatable {
    let name: String
    let url: URL
    var id: String { name }
}

/// Finds daemons that advertise `_fsonos._tcp` over Bonjour and resolves each to an `http://<ipv4>:<port>` URL.
/// An address is used instead of the `.local` name because iOS resolves names through the system resolver, which a
/// tailnet or exit node can break.
@MainActor
final class DaemonBrowser: ObservableObject {
    static let serviceType = "_fsonos._tcp"

    @Published private(set) var found: [DiscoveredDaemon] = []
    private var browser: NWBrowser?
    private var resolvers: [String: ServiceResolver] = [:]

    func start() {
        guard browser == nil else { return }
        let browser = NWBrowser(for: .bonjour(type: Self.serviceType, domain: nil), using: .tcp)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            let services = results.compactMap { result -> ServiceName? in
                guard case let .service(name, type, domain, _) = result.endpoint else { return nil }
                return ServiceName(name: name, type: type, domain: domain)
            }
            Task { @MainActor in self?.update(services) }
        }
        browser.start(queue: .main)
        self.browser = browser
    }

    func stop() {
        browser?.cancel()
        browser = nil
        resolvers.values.forEach { $0.cancel() }
        resolvers = [:]
    }

    private func update(_ services: [ServiceName]) {
        let names = Set(services.map(\.name))
        found.removeAll { !names.contains($0.name) }
        for name in resolvers.keys where !names.contains(name) {
            resolvers[name]?.cancel()
            resolvers[name] = nil
        }
        for service in services where resolvers[service.name] == nil && !found.contains(where: { $0.name == service.name }) {
            resolvers[service.name] = ServiceResolver(service) { [weak self] host, port in
                Task { @MainActor in self?.resolved(service.name, host: host, port: port) }
            }
        }
    }

    private func resolved(_ name: String, host: String, port: Int) {
        resolvers[name] = nil
        guard let url = URL(string: "http://\(host):\(port)"), !found.contains(where: { $0.name == name }) else { return }
        AppLog.shared.add("bonjour", "found \(name) at \(url.absoluteString)")
        found.append(DiscoveredDaemon(name: name, url: url))
        found.sort { $0.name < $1.name }
    }
}

struct ServiceName {
    let name: String
    let type: String
    let domain: String
}

/// Reads one service's SRV and A records. NetService is deprecated but is the call that returns the address records
/// without opening a connection; resolving through NWConnection waits for a reachable peer and hangs on a LAN address
/// the process may not reach yet.
final class ServiceResolver: NSObject, NetServiceDelegate {
    private let service: NetService
    private let done: (String, Int) -> Void

    init(_ name: ServiceName, done: @escaping (String, Int) -> Void) {
        service = NetService(domain: name.domain, type: name.type, name: name.name)
        self.done = done
        super.init()
        service.delegate = self
        service.resolve(withTimeout: 10)
    }

    func cancel() { service.stop() }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        AppLog.shared.add("bonjour", "could not resolve \(sender.name): \(errorDict)")
    }

    /// An IPv4 address when the service has one; otherwise its `.local` host name, so a host that publishes only IPv6
    /// link-local addresses is still offered.
    func netServiceDidResolveAddress(_ sender: NetService) {
        if let address = (sender.addresses ?? []).lazy.compactMap(Self.ipv4).first {
            done(address, sender.port)
            return
        }
        let host = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")) ?? ""
        if !host.isEmpty { done(host, sender.port) }
    }

    private static func ipv4(_ data: Data) -> String? {
        data.withUnsafeBytes { raw -> String? in
            guard let base = raw.baseAddress else { return nil }
            let address = base.assumingMemoryBound(to: sockaddr.self)
            guard address.pointee.sa_family == sa_family_t(AF_INET) else { return nil }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(address, socklen_t(data.count), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 else { return nil }
            return String(cString: host)
        }
    }
}
