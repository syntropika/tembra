import Foundation

/**
 * One isolated revision and the single byte buffer its last received ingress event left for the
 * synchronous take. The buffer is created off the JavaScript thread and handed over exactly once.
 */
final class PortSlot {
    let port: NativePort
    var pending: NSMutableData?
    var receiving = false
    init(_ port: NativePort) { self.port = port }
}

/** Own prepared artifacts and isolated revisions for exactly one mobile host. */
final class Execution: @unchecked Sendable {
    private let lock = NSRecursiveLock()
    private var closed = false
    private var engine: NativeInterpreter?
    private var artifacts: [String: NativeArtifact] = [:]
    private var ports: [String: PortSlot] = [:]

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
        try live { ports[id] = PortSlot(port) }
        return id
    }

    /** Resolve an isolated revision without retaining a table lock across awaits. */
    func port(_ id: String) throws -> NativePort {
        try live { guard let slot = ports[id] else { throw MobileError.Closed }; return slot.port }
    }

    /** Return checked executable metadata. */
    func description(_ id: String) throws -> String {
        try live {
            guard let slot = ports[id] else { throw MobileError.Closed }
            return try slot.port.description()
        }
    }

    /** Drain host-attributed diagnostics. */
    func logs(_ id: String) throws -> String {
        try live {
            guard let slot = ports[id] else { throw MobileError.Closed }
            return slot.port.takeLogs()
        }
    }

    /** Retain a revision if physical work prevents it from closing yet. */
    func closePort(_ id: String) throws {
        try live {
            if let slot = ports[id] { try slot.port.shutdown(); ports.removeValue(forKey: id) }
        }
    }

    /**
     * Receive the next ingress event. Its metadata resolves only after any bytes are parked in the
     * port's slot, and a receive is refused while an earlier event's bytes are still untaken.
     */
    func receiveIngress(_ id: String) async throws -> String {
        let slot = try live { () throws -> PortSlot in
            guard let slot = ports[id] else { throw MobileError.Closed }
            guard !slot.receiving, slot.pending == nil else {
                throw MobileError.Execution(diagnostic: "The previous ingress event has not been taken")
            }
            slot.receiving = true
            return slot
        }
        let metadata: String, bytes: NSMutableData?
        do { (metadata, bytes) = try IngressCodec.event(try await slot.port.receiveIngress()) }
        catch { settle(slot); throw error }
        return try live {
            slot.receiving = false
            guard ports[id] === slot else { throw MobileError.Closed }
            slot.pending = bytes
            return metadata
        }
    }

    /** Reopen a slot whose receive failed; synchronous so the lock is never held across a suspension. */
    private func settle(_ slot: PortSlot) {
        lock.lock()
        defer { lock.unlock() }
        slot.receiving = false
    }

    /** Hand the pending bytes of the last received event to JavaScript exactly once, never waiting. */
    func takeIngress(_ id: String) throws -> NSMutableData {
        try live {
            guard let slot = ports[id] else { throw MobileError.Closed }
            guard let bytes = slot.pending else {
                throw MobileError.Execution(diagnostic: "No received ingress event has pending bytes")
            }
            slot.pending = nil
            return bytes
        }
    }

    /** Drop platform owners during invalidation; native tickets drain their own physical work. */
    func close() {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        closed = true
        let pendingPorts = ports.values.map(\.port)
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

/** Translate between typed UniFFI ingress values and bridge metadata plus one byte buffer. */
enum IngressCodec {
    private static func invalid(_ what: String) -> MobileError { MobileError.Execution(diagnostic: "Invalid ingress \(what)") }

    private static func json(_ value: Any) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed]), as: UTF8.self)
    }

    private static func object(_ text: String) throws -> [String: Any] {
        guard let value = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any] else { throw invalid("metadata") }
        return value
    }

    /** Names and lengths for metadata; values concatenated in order, duplicates kept. */
    private static func pack(_ fields: [MobileField]) -> ([[Any]], NSMutableData) {
        let bytes = NSMutableData(capacity: fields.reduce(0) { $0 + $1.value.count }) ?? NSMutableData()
        for field in fields { bytes.append(field.value) }
        return (fields.map { [$0.name, $0.value.count] }, bytes)
    }

    /** Split one buffer by the metadata's names and lengths, which must cover it exactly. */
    static func fields(_ metadata: Any?, _ bytes: Data) throws -> [MobileField] {
        guard let entries = metadata as? [[Any]] else { throw invalid("fields") }
        var offset = 0
        let fields = try entries.map { entry -> MobileField in
            guard entry.count == 2, let name = entry[0] as? String, let length = (entry[1] as? NSNumber)?.intValue,
                  length >= 0, length <= bytes.count - offset else { throw invalid("field") }
            defer { offset += length }
            return MobileField(name: name, value: bytes.subdata(in: (bytes.startIndex + offset)..<(bytes.startIndex + offset + length)))
        }
        guard offset == bytes.count else { throw invalid("field lengths") }
        return fields
    }

    static func head(_ text: String, _ bytes: Data) throws -> MobileRequestHead {
        let head = try object(text)
        guard let method = head["method"] as? String else { throw invalid("method") }
        return MobileRequestHead(
            method: method,
            scheme: head["scheme"] as? String,
            authority: head["authority"] as? String,
            pathWithQuery: head["pathWithQuery"] as? String,
            fields: try fields(head["fields"], bytes))
    }

    static func chunk(_ text: String, _ bytes: Data) throws -> MobileRequestChunk {
        let chunk = try object(text)
        switch chunk["kind"] as? String {
        case "data": return .data(bytes: bytes)
        case "end": return .end(trailers: try fields(chunk["trailers"], bytes))
        case "error":
            guard bytes.isEmpty, let message = chunk["message"] as? String else { throw invalid("error chunk") }
            return .error(message: message)
        default: throw invalid("chunk")
        }
    }

    /** Metadata for JavaScript and, for byte events, the one buffer it must take. */
    static func event(_ event: MobileIngressEvent) throws -> (String, NSMutableData?) {
        switch event {
        case let .host(id, asynchronous, packet):
            return (try json(["kind": "host", "id": id, "asynchronous": asynchronous, "packet": packet]), nil)
        case let .complete(result): return (try json(["kind": "complete", "result": result]), nil)
        case let .head(status, fields):
            let (names, bytes) = pack(fields)
            return (try json(["kind": "head", "status": status, "fields": names]), bytes)
        case .requestPull: return (#"{"kind":"requestPull"}"#, nil)
        case let .data(bytes): return (try json(["kind": "data", "length": bytes.count]), NSMutableData(data: bytes))
        case let .trailers(fields):
            let (names, bytes) = pack(fields)
            return (try json(["kind": "trailers", "fields": names]), bytes)
        case .end: return (#"{"kind":"end"}"#, nil)
        case let .error(message): return (try json(["kind": "error", "message": message]), nil)
        case let .refused(code): return (try json(["kind": "refused", "length": code.count]), NSMutableData(data: code))
        }
    }
}
