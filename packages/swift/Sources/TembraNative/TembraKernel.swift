import Foundation

/** Exchange metadata with one Rust kernel without a React Native runtime. */
public final class TembraKernel: @unchecked Sendable {
    private let kernel: NativeKernel

    /** Create an isolated kernel on its owning worker thread. */
    public init() throws {
        kernel = try NativeKernel()
    }

    /** Execute one metadata command and preserve its result envelope. */
    public func request(_ metadata: String) throws -> String {
        try kernel.request(metadata: metadata)
    }

    /** Stop the worker after the application has completed lifecycle cleanup. */
    public func close() {
        kernel.shutdown()
    }

    /** Stop the worker when its Swift owner is released. */
    deinit { close() }
}
