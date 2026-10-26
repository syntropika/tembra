import Foundation

/** Own native kernels independently of the React Native calling thread. */
@objc(Tembra) public final class Tembra: NSObject {
    private let lock = NSLock()
    private var kernels: [String: NativeKernel] = [:]
    private var invalidated = false

    /** Allocate an isolated native kernel for one JavaScript host. */
    @objc public func createKernel() throws -> String {
        lock.lock()
        defer { lock.unlock() }
        guard !invalidated else { throw MobileError.Closed }
        let handle = UUID().uuidString
        kernels[handle] = try NativeKernel()
        return handle
    }

    /** Forward metadata while excluding concurrent invalidation. */
    @objc public func request(_ handle: String, metadata: String) throws -> String {
        lock.lock()
        defer { lock.unlock() }
        guard let kernel = kernels[handle] else { throw MobileError.Closed }
        return try kernel.request(metadata: metadata)
    }

    /** Release one kernel after the host has drained its lifecycle. */
    @objc public func releaseKernel(_ handle: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let kernel = kernels.removeValue(forKey: handle) else { return false }
        kernel.shutdown()
        return true
    }

    /** Release every worker when React Native destroys the module. */
    @objc public func invalidate() {
        lock.lock()
        defer { lock.unlock() }
        invalidated = true
        for kernel in kernels.values { kernel.shutdown() }
        kernels.removeAll()
    }

    /** Stop workers when the module is deallocated without explicit invalidation. */
    deinit { invalidate() }
}
