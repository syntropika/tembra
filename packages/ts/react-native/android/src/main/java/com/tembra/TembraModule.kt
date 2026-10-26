package com.tembra

import com.facebook.react.bridge.ReactApplicationContext
import java.util.UUID

/** Own kernels for one React Native module, including reload cleanup. */
class TembraModule(context: ReactApplicationContext) : NativeTembraSpec(context) {
    private val kernels = mutableMapOf<String, TembraKernel>()
    private var invalidated = false

    /** Return the name used by the generated JavaScript specification. */
    override fun getName() = NAME

    /** Allocate an isolated Rust worker for one host. */
    @Synchronized
    override fun createKernel(): String {
        check(!invalidated) { "The native module has been invalidated" }
        val handle = UUID.randomUUID().toString()
        kernels[handle] = TembraKernel()
        return handle
    }

    /** Exchange metadata synchronously without retaining application values. */
    @Synchronized
    override fun request(handle: String, metadata: String): String {
        val kernel = checkNotNull(kernels[handle]) { "The native kernel has been released" }
        return kernel.request(metadata)
    }

    /** Release native storage after JavaScript lifecycle cleanup. */
    @Synchronized
    override fun releaseKernel(handle: String): Boolean {
        val kernel = kernels.remove(handle) ?: return false
        kernel.close()
        return true
    }

    /** Stop all workers when the React Native instance is destroyed. */
    @Synchronized
    override fun invalidate() {
        invalidated = true
        kernels.values.forEach { kernel ->
            kernel.close()
        }
        kernels.clear()
        super.invalidate()
    }

    companion object {
        const val NAME = "NativeTembra"
    }
}
