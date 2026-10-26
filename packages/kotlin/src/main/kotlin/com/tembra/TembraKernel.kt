package com.tembra

import com.tembra.ffi.NativeKernel

/** Exchange metadata with one Rust kernel without a React Native runtime. */
class TembraKernel : AutoCloseable {
    private val kernel = NativeKernel()
    private var closed = false

    /** Execute one metadata command and preserve its result envelope. */
    @Synchronized
    fun request(metadata: String): String {
        check(!closed) { "The native kernel has been released" }
        return kernel.request(metadata)
    }

    /** Stop the worker and release its foreign handle once. */
    @Synchronized
    override fun close() {
        if (closed) return
        kernel.shutdown()
        kernel.close()
        closed = true
    }
}
