package com.tembra

import com.facebook.react.bridge.ArrayBuffer
import com.facebook.react.bridge.ReactApplicationContext
import java.util.UUID
import android.util.Base64
import com.facebook.react.bridge.Promise
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/** Own kernels for one React Native module, including reload cleanup. */
class TembraModule(private val context: ReactApplicationContext) : NativeTembraSpec(context) {
    private val kernels = mutableMapOf<String, TembraKernel>()
    private val executions = mutableMapOf<String, Execution>()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var invalidated = false

    /** Return the name used by the generated JavaScript specification. */
    override fun getName() = NAME

    /** Allocate an isolated Rust worker for one host. */
    @Synchronized
    override fun createKernel(): String {
        check(!invalidated) { "The native module has been invalidated" }
        val handle = UUID.randomUUID().toString()
        kernels[handle] = nativeCall { TembraKernel() }
        executions[handle] = Execution()
        return handle
    }

    /** Exchange metadata synchronously without retaining application values. */
    @Synchronized
    override fun request(handle: String, metadata: String): String {
        val kernel = checkNotNull(kernels[handle]) { "The native kernel has been released" }
        return nativeCall { kernel.request(metadata) }
    }

    /** Release native storage after JavaScript lifecycle cleanup. */
    @Synchronized
    override fun releaseKernel(handle: String): Boolean {
        val kernel = kernels.remove(handle) ?: return false
        executions.remove(handle)?.close()
        kernel.close()
        return true
    }

    /** Stop all workers when the React Native instance is destroyed. */
    @Synchronized
    override fun invalidate() {
        invalidated = true
        scope.cancel()
        executions.values.forEach { it.close() }
        executions.clear()
        kernels.values.forEach { kernel ->
            kernel.close()
        }
        kernels.clear()
        super.invalidate()
    }

    /** Snapshot one host-owned table without holding the module lock across awaits. */
    @Synchronized private fun execution(handle: String): Execution = checkNotNull(executions[handle]) { "The native host has been released" }

    /** Preserve generated Rust error diagnostics across both TurboModule call styles. */
    private fun diagnostic(error: Throwable): String = when (error) {
        is com.tembra.ffi.MobileException.Execution -> error.diagnostic
        is com.tembra.ffi.MobileException.Closed -> "The native kernel has been released"
        is com.tembra.ffi.MobileException.WorkerUnavailable -> "The native kernel worker is unavailable"
        else -> error.message ?: error.toString()
    }

    private fun <T> nativeCall(action: () -> T): T = try { action() }
        catch (error: com.tembra.ffi.MobileException) { throw IllegalStateException(diagnostic(error), error) }

    /** Resolve or reject each native promise on physical completion. */
    private fun promise(promise: Promise, action: suspend () -> Any?) {
        scope.launch { try { promise.resolve(action()) } catch (error: Throwable) { promise.reject("tembra_execution", diagnostic(error), error) } }
    }

    /** Resolve app-private storage before granting any Orb a mount. */
    override fun filesystemDirectory(kind: String): String {
        val base = when (kind) { "application" -> context.filesDir; "temporary" -> context.cacheDir; else -> error("Unknown filesystem adapter") }
        val directory = java.io.File(base, "tembra")
        check(directory.isDirectory || directory.mkdirs()) { "Cannot create filesystem adapter directory" }
        return directory.absolutePath
    }

    /** Load the packaged interpreter unless an explicit engine was supplied. */
    override fun configureExecution(handle: String, engine: String, configuration: String, promise: Promise) = promise(promise) {
        val bytes = if (engine.isEmpty()) context.assets.open("tembra/interpreter.wasm").use { it.readBytes() } else Base64.decode(engine, Base64.NO_WRAP)
        execution(handle).configure(configuration, bytes)
        null
    }
    /** Decode prepared artifacts into host-owned handles. */
    override fun createArtifact(handle: String, bytes: String): String = nativeCall { execution(handle).createArtifact(Base64.decode(bytes, Base64.NO_WRAP)) }
    /** Read the artifact identity. */
    override fun artifactDescription(handle: String, artifact: String): String = nativeCall { execution(handle).artifact(artifact).description() }
    /** Query checked installation metadata. */
    override fun artifactRequest(handle: String, artifact: String, packet: String): String = nativeCall { execution(handle).artifact(artifact).request(packet) }
    /** Bind one checked value signature. */
    override fun artifactBind(handle: String, artifact: String, binding: String, method: String, exports: Boolean): Double = nativeCall { execution(handle).artifact(artifact).bind(binding, method, exports).toDouble() }
    /** Check captured bytes against a bound slot. */
    override fun artifactValidate(handle: String, artifact: String, slot: Double, arguments_: Boolean, value: String): String = nativeCall { execution(handle).artifact(artifact).validate(slot.toUInt(), arguments_, value) }
    /** Release a prepared contract owner. */
    override fun releaseArtifact(handle: String, artifact: String): Boolean = nativeCall { execution(handle).releaseArtifact(artifact); true }
    /** Open a source or compiled revision away from the JS event loop. */
    override fun openPort(handle: String, artifact: String, bindings: String, grants: String, cli: String, promise: Promise) = promise(promise) { execution(handle).open(artifact, bindings, grants, cli) }
    /** Read checked executable metadata. */
    override fun portDescription(handle: String, port: String): String = nativeCall { execution(handle).description(port) }
    /** Transfer host-attributed guest diagnostics. */
    override fun portLogs(handle: String, port: String): String = nativeCall { execution(handle).logs(port) }
    /** Start an issued declaration or callback. */
    override fun portBegin(handle: String, port: String, packet: String, declaration: Boolean): Boolean = nativeCall { execution(handle).port(port).begin(packet, declaration); true }
    /** Await physical requests without a waiting platform thread. */
    override fun portReceive(handle: String, port: String, promise: Promise) = promise(promise) { execution(handle).port(port).receive() }
    /** Resolve the exact request issued by this revision. */
    override fun portReply(handle: String, port: String, id: Double, packet: String): Boolean = nativeCall { execution(handle).port(port).reply(id.toUInt(), packet) }
    /** Release the physical admission ticket. */
    override fun portFinish(handle: String, port: String): Boolean = nativeCall { execution(handle).port(port).finish(); true }
    /** Quarantine a failed exchange away from the JS and UI threads. */
    override fun abortPort(handle: String, port: String, promise: Promise) = promise(promise) { execution(handle).port(port).invalidate(); null }
    /** Drop drained guest environment bookkeeping. */
    override fun portRelease(handle: String, port: String, environment: Double): Boolean = nativeCall { execution(handle).port(port).release(environment.toULong()); true }
    /** Cancel standard WASI work owned by the active callback. */
    override fun portCancel(handle: String, port: String): Boolean = nativeCall { execution(handle).port(port).cancel(); true }
    /** Start an incoming HTTP exchange; the head's field values arrive as one typed buffer. */
    override fun portBeginIngress(handle: String, port: String, packet: String, head: String, fields: ArrayBuffer): Boolean =
        nativeCall { execution(handle).port(port).beginIngress(packet, IngressCodec.head(head, IngressCodec.copy(fields))); true }
    /** Await the next ingress event; byte events park their buffer for the synchronous take. */
    override fun portReceiveIngress(handle: String, port: String, promise: Promise) = promise(promise) { execution(handle).receiveIngress(port) }
    /** Move the received event's owning buffer to JavaScript. */
    override fun portTakeIngress(handle: String, port: String): ArrayBuffer = nativeCall { execution(handle).takeIngress(port) }
    /** Answer the pending request pull with typed bytes. */
    override fun portSupplyRequest(handle: String, port: String, chunk: String, bytes: ArrayBuffer): Boolean =
        nativeCall { execution(handle).port(port).supplyRequest(IngressCodec.chunk(chunk, IngressCodec.copy(bytes))) }
    /** Ask for the next response frame. */
    override fun portPullResponse(handle: String, port: String): Boolean = nativeCall { execution(handle).port(port).pullResponse(); true }
    /** Drop the response because the client went away. */
    override fun portDisconnect(handle: String, port: String): Boolean = nativeCall { execution(handle).port(port).disconnect(); true }
    /** Close a revision after shared lifecycle cleanup. */
    override fun closePort(handle: String, port: String): Boolean = nativeCall { execution(handle).closePort(port); true }

    companion object {
        const val NAME = "NativeTembra"
    }
}
