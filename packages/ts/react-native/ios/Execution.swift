import Foundation

/** Own prepared artifacts and isolated revisions for exactly one mobile host. */
final class Execution: @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var closed = false
    private var engine: NativeInterpreter?
    private var artifacts: [String: NativeArtifact] = [:]
    private var ports: [String: NativePort] = [:]

    /** Serialize table access and reject operations after host invalidation. */
    private func live<T>(_ operation: () throws -> T) throws -> T {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { throw MobileError.Closed }
        return try operation()
    }

    /** Configure the portable engine outside the table lock, then adopt it only into a live host. */
    func configure(_ configuration: String, bytes: Data) throws {
        let candidate = try NativeInterpreter(configuration: configuration, interpreter: bytes)
        try live {
            guard engine == nil else { throw MobileError.Execution(diagnostic: "Execution is already configured") }
            engine = candidate
        }
    }

    /** Decode an artifact before assigning an opaque identity local to this host. */
    func createArtifact(_ bytes: Data) throws -> String {
        let artifact = try NativeArtifact(bytes: bytes)
        return try live { let id = UUID().uuidString; artifacts[id] = artifact; return id }
    }

    /** Resolve only an artifact issued by this host. */
    func artifact(_ id: String) throws -> NativeArtifact {
        try live {
            guard let artifact = artifacts[id] else { throw MobileError.Closed }
            return artifact
        }
    }

    /** Release a prepared contract after its revision has stopped using it. */
    func releaseArtifact(_ id: String) throws { _ = try live { artifacts.removeValue(forKey: id) } }

    /** Open one fresh source or compiled Store after checking installation authority. */
    func open(_ id: String, bindings: String, grants: String, cli: String) throws -> String {
        let (engine, artifact) = try live { () throws -> (NativeInterpreter, NativeArtifact) in
            guard let engine, let artifact = artifacts[id] else { throw MobileError.Closed }
            return (engine, artifact)
        }
        let id = UUID().uuidString
        let port = try engine.open(artifact: artifact, bindings: bindings, grants: grants, cli: cli)
        try live { ports[id] = port }
        return id
    }

    /** Resolve an isolated revision without retaining a table lock across awaits. */
    func port(_ id: String) throws -> NativePort {
        try live { guard let port = ports[id] else { throw MobileError.Closed }; return port }
    }

    /** Return checked executable metadata. */
    func description(_ id: String) throws -> String {
        try live {
            guard let port = ports[id] else { throw MobileError.Closed }
            return try port.description()
        }
    }

    /** Drain host-attributed diagnostics. */
    func logs(_ id: String) throws -> String {
        try live {
            guard let port = ports[id] else { throw MobileError.Closed }
            return port.takeLogs()
        }
    }

    /** Retain a revision if physical work prevents it from closing yet. */
    func closePort(_ id: String) throws {
        try live {
            if let port = ports[id] { try port.shutdown(); ports.removeValue(forKey: id) }
        }
    }

    /** Drop platform owners during invalidation; native tickets drain their own physical work. */
    func close() {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        closed = true
        let pendingPorts = Array(ports.values)
        let pendingArtifacts = Array(artifacts.values)
        let pendingEngine = engine
        ports.removeAll()
        artifacts.removeAll()
        engine = nil
        lock.unlock()
        DispatchQueue.global(qos: .utility).async {
            for port in pendingPorts { port.invalidate() }
            withExtendedLifetime((pendingPorts, pendingArtifacts, pendingEngine)) {}
        }
    }
}
