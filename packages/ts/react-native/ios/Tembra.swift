import Foundation

/** Own native kernels independently of the React Native calling thread. */
@objc(Tembra) public final class Tembra: NSObject {
    private let lock = NSLock()
    private var kernels: [String: TembraKernel] = [:]
    private var executions: [String: Execution] = [:]
    private var invalidated = false

    /** Allocate an isolated native kernel for one JavaScript host. */
    @objc public func createKernel() throws -> String {
        lock.lock()
        defer { lock.unlock() }
        guard !invalidated else {
            throw NSError(domain: "NativeTembra", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "The native module has been invalidated"
            ])
        }
        let handle = UUID().uuidString
        kernels[handle] = try checked { try TembraKernel() }
        executions[handle] = Execution()
        return handle
    }

    /** Forward metadata while excluding concurrent invalidation. */
    @objc public func request(_ handle: String, metadata: String) throws -> String {
        lock.lock()
        defer { lock.unlock() }
        guard let kernel = kernels[handle] else {
            throw NSError(domain: "NativeTembra", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "The native kernel has been released"
            ])
        }
        return try checked { try kernel.request(metadata) }
    }

    /** Release one kernel after the host has drained its lifecycle. */
    @objc public func releaseKernel(_ handle: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let kernel = kernels.removeValue(forKey: handle) else { return false }
        executions.removeValue(forKey: handle)?.close()
        kernel.close()
        return true
    }

    /** Release every worker when React Native destroys the module. */
    @objc public func invalidate() {
        lock.lock()
        defer { lock.unlock() }
        invalidated = true
        for execution in executions.values { execution.close() }
        executions.removeAll()
        for kernel in kernels.values { kernel.close() }
        kernels.removeAll()
    }

    /** Stop workers when the module is deallocated without explicit invalidation. */
    deinit { invalidate() }
}


extension Tembra {
    /** Resolve a host-owned execution table without holding the module lock during I/O. */
    private func execution(_ handle: String) throws -> Execution {
        lock.lock()
        defer { lock.unlock() }
        guard let execution = executions[handle] else { throw MobileError.Closed }
        return execution
    }

    /** Keep native rejection text consistent across generated language bindings. */
    private func diagnostic(_ error: Error) -> String {
        switch error {
        case MobileError.Execution(let diagnostic): return diagnostic
        case MobileError.Closed: return "The native kernel has been released"
        case MobileError.WorkerUnavailable: return "The native kernel worker is unavailable"
        default: return error.localizedDescription
        }
    }

    /** Preserve the same diagnostic when Objective-C translates a synchronous exception. */
    private func checked<T>(_ operation: () throws -> T) throws -> T {
        do { return try operation() }
        catch { throw NSError(domain: "NativeTembra", code: 1, userInfo: [NSLocalizedDescriptionKey: diagnostic(error)]) }
    }

    /** Resolve app-owned storage before any Orb receives a mount grant. */
    @objc public func filesystemDirectory(_ kind: String) throws -> String {
        try checked {
            let manager = FileManager.default
            let base: URL
            switch kind {
            case "application": base = try manager.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            case "temporary": base = manager.temporaryDirectory
            default: throw MobileError.Execution(diagnostic: "Unknown filesystem adapter")
            }
            let directory = base.appendingPathComponent("tembra", isDirectory: true)
            try manager.createDirectory(at: directory, withIntermediateDirectories: true)
            return directory.path
        }
    }

    /** Decode the TurboModule byte transport strictly. */
    private func bytes(_ value: String) throws -> Data {
        guard let data = Data(base64Encoded: value) else { throw MobileError.Execution(diagnostic: "Invalid base64 bytes") }
        return data
    }

    /** Load the packaged interpreter unless the host supplies explicit engine bytes. */
    @objc public func configureExecution(_ handle: String, engine: String, configuration: String, resolve: @escaping (String) -> Void, reject: @escaping (String) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            do {
                let data: Data
                if engine.isEmpty {
                    guard let bundleURL = Bundle(for: Tembra.self).url(forResource: "TembraInterpreter", withExtension: "bundle"),
                          let bundle = Bundle(url: bundleURL),
                          let url = bundle.url(forResource: "interpreter", withExtension: "wasm") else {
                        throw MobileError.Execution(diagnostic: "The packaged interpreter is missing; rebuild the native library")
                    }
                    data = try Data(contentsOf: url)
                } else { data = try bytes(engine) }
                try execution(handle).configure(configuration, bytes: data)
                resolve("")
            } catch { reject(diagnostic(error)) }
        }
    }

    /** Decode source or compiled artifacts without executing them in the host realm. */
    @objc public func createArtifact(_ handle: String, bytes value: String) throws -> String { try checked { try execution(handle).createArtifact(bytes(value)) } }
    /** Read the artifact's canonical identity. */
    @objc public func artifactDescription(_ handle: String, artifact: String) throws -> String { try checked { try execution(handle).artifact(artifact).description() } }
    /** Query the prepared contract, including imports and grants. */
    @objc public func artifactRequest(_ handle: String, artifact: String, packet: String) throws -> String { try checked { try execution(handle).artifact(artifact).request(packet: packet) } }
    /** Bind one prepared method signature. */
    @objc public func artifactBind(_ handle: String, artifact: String, binding: String, method: String, exports: Bool) throws -> NSNumber { try checked { return NSNumber(value: try execution(handle).artifact(artifact).bind(binding: binding, method: method, exports: exports)) } }
    /** Validate exact captured bytes before a guest boundary. */
    @objc public func artifactValidate(_ handle: String, artifact: String, slot: NSNumber, arguments: Bool, value: String) throws -> String { try checked { try execution(handle).artifact(artifact).validate(slot: slot.uint32Value, arguments: arguments, value: value) } }
    /** Release one host-owned prepared artifact. */
    @objc public func releaseArtifact(_ handle: String, artifact: String) throws -> NSNumber { try checked { try execution(handle).releaseArtifact(artifact); return NSNumber(value: true) } }
    /** Prepare a fresh Store without blocking the JavaScript event loop. */
    @objc public func openPort(_ handle: String, artifact: String, bindings: String, grants: String, cli: String, resolve: @escaping (String) -> Void, reject: @escaping (String) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async { [self] in do { resolve(try execution(handle).open(artifact, bindings: bindings, grants: grants, cli: cli)) } catch { reject(diagnostic(error)) } }
    }
    /** Read checked executable metadata. */
    @objc public func portDescription(_ handle: String, port: String) throws -> String { try checked { try execution(handle).description(port) } }
    /** Transfer bounded, host-attributed guest diagnostics. */
    @objc public func portLogs(_ handle: String, port: String) throws -> String { try checked { try execution(handle).logs(port) } }
    /** Start a factory or an admitted callback. */
    @objc public func portBegin(_ handle: String, port: String, packet: String, declaration: Bool) throws -> NSNumber { try checked { try execution(handle).port(port).begin(packet: packet, declaration: declaration); return NSNumber(value: true) } }
    /** Await native events without blocking a platform worker. */
    @objc public func portReceive(_ handle: String, port: String, resolve: @escaping (String) -> Void, reject: @escaping (String) -> Void) {
        Task { do { resolve(try await execution(handle).port(port).receive()) } catch { reject(diagnostic(error)) } }
    }
    /** Resolve only an issued native host request. */
    @objc public func portReply(_ handle: String, port: String, id: NSNumber, packet: String) throws -> NSNumber { try checked { return NSNumber(value: try execution(handle).port(port).reply(id: id.uint32Value, packet: packet)) } }
    /** Release the physical admission ticket after completion. */
    @objc public func portFinish(_ handle: String, port: String) throws -> NSNumber { try checked { try execution(handle).port(port).finish(); return NSNumber(value: true) } }
    /** Quarantine a failed exchange and await physical shutdown away from the JS thread. */
    @objc public func abortPort(_ handle: String, port: String, resolve: @escaping (String) -> Void, reject: @escaping (String) -> Void) {
        DispatchQueue.global(qos: .utility).async { [self] in
            do { try execution(handle).port(port).invalidate(); resolve("") }
            catch { reject(diagnostic(error)) }
        }
    }
    /** Release guest-owned environment bookkeeping. */
    @objc public func portRelease(_ handle: String, port: String, environment: NSNumber) throws -> NSNumber { try checked { try execution(handle).port(port).release(environment: environment.uint64Value); return NSNumber(value: true) } }
    /** Run capability-bound native filesystem or transport work. */
    @objc public func portIo(_ handle: String, port: String, packet: String, resolve: @escaping (String) -> Void, reject: @escaping (String) -> Void) {
        Task { do { resolve(try await execution(handle).port(port).io(packet: packet)) } catch { reject(diagnostic(error)) } }
    }
    /** Revoke the active callback's source I/O after its caller was cancelled. */
    @objc public func portCancelIo(_ handle: String, port: String) throws -> NSNumber { try checked { try execution(handle).port(port).cancelIo(); return NSNumber(value: true) } }
    /** Close a revision after shared lifecycle cleanup. */
    @objc public func closePort(_ handle: String, port: String) throws -> NSNumber { try checked { try execution(handle).closePort(port); return NSNumber(value: true) } }
}
