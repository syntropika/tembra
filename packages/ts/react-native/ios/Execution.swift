import Foundation

/** Own prepared artifacts and isolated revisions for exactly one mobile host. */
final class Execution: @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var closed = false
    private var engine: NativeInterpreter?
    private var artifacts: [String: NativeArtifact] = [:]
    private var sources: [String: NativePort] = [:]
    private var components: [String: NativeComponent] = [:]

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
    func open(_ id: String, bindings: String, grants: String, component: Bool) throws -> String {
        let (engine, artifact) = try live { () throws -> (NativeInterpreter, NativeArtifact) in
            guard let engine, let artifact = artifacts[id] else { throw MobileError.Closed }
            return (engine, artifact)
        }
        let id = UUID().uuidString
        if component {
            let port = try engine.openComponent(artifact: artifact, bindings: bindings, grants: grants)
            try live { components[id] = port }
        } else {
            let port = try engine.open(artifact: artifact, bindings: bindings, grants: grants)
            try live { sources[id] = port }
        }
        return id
    }

    /** Resolve an isolated source revision without retaining a table lock across awaits. */
    func source(_ id: String) throws -> NativePort {
        try live { guard let port = sources[id] else { throw MobileError.Closed }; return port }
    }

    /** Resolve a compiled revision only from its owning host. */
    func component(_ id: String) throws -> NativeComponent {
        try live { guard let port = components[id] else { throw MobileError.Closed }; return port }
    }

    /** Return checked executable metadata for either implementation language. */
    func description(_ id: String) throws -> String {
        try live {
            if let port = sources[id] { return try port.description() }
            if let port = components[id] { return try port.description() }
            throw MobileError.Closed
        }
    }

    /** Drain host-attributed diagnostics for either implementation. */
    func logs(_ id: String) throws -> String {
        try live {
            if let port = sources[id] { return port.takeLogs() }
            if let port = components[id] { return port.takeLogs() }
            throw MobileError.Closed
        }
    }

    /** Retain a source revision if physical work prevents it from closing yet. */
    func closePort(_ id: String) throws {
        try live {
            if let port = sources[id] { try port.shutdown(); sources.removeValue(forKey: id) }
            if let port = components.removeValue(forKey: id) { port.shutdown() }
        }
    }

    /** Drop platform owners during invalidation; native tickets drain their own physical work. */
    func close() {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        closed = true
        let pendingSources = Array(sources.values)
        let pendingComponents = Array(components.values)
        let pendingArtifacts = Array(artifacts.values)
        let pendingEngine = engine
        sources.removeAll()
        components.removeAll()
        artifacts.removeAll()
        engine = nil
        lock.unlock()
        DispatchQueue.global(qos: .utility).async {
            for port in pendingSources { port.invalidate() }
            for port in pendingComponents { port.shutdown() }
            withExtendedLifetime((pendingSources, pendingComponents, pendingArtifacts, pendingEngine)) {}
        }
    }
}
