package com.tembra

import com.tembra.ffi.NativeArtifact
import com.tembra.ffi.NativeInterpreter
import com.tembra.ffi.NativePort
import java.util.UUID

/** Own opaque execution identities for one host, independently of foreign callback threads. */
internal class Execution : AutoCloseable {
    private var closed = false
    private var engine: NativeInterpreter? = null
    private val artifacts = mutableMapOf<String, NativeArtifact>()
    private val ports = mutableMapOf<String, NativePort>()

    /** Refuse publication or access after the host has been invalidated. */
    @Synchronized private fun <T> live(action: () -> T): T {
        check(!closed) { "The native host has been released" }
        return action()
    }

    /** Prepare portable execution outside the table lock and adopt it only into a live host. */
    fun configure(configuration: String, bytes: ByteArray) {
        val candidate = NativeInterpreter(configuration, bytes)
        try {
            live { check(engine == null) { "Execution is already configured" }; engine = candidate }
        } catch (error: Throwable) { candidate.close(); throw error }
    }

    /** Decode a prepared artifact without evaluating its implementation. */
    fun createArtifact(bytes: ByteArray): String {
        val candidate = NativeArtifact(bytes)
        try { return live { val id = UUID.randomUUID().toString(); artifacts[id] = candidate; id } }
        catch (error: Throwable) { candidate.close(); throw error }
    }

    /** Resolve only artifacts issued by this host. */
    fun artifact(id: String): NativeArtifact = live { checkNotNull(artifacts[id]) { "Unknown artifact" } }

    /** Release the foreign prepared-contract owner after shared lifecycle cleanup. */
    fun releaseArtifact(id: String) { live { artifacts.remove(id)?.close() } }

    /** Create a new isolated source or compiled Store and retain it only while its host remains live. */
    fun open(id: String, bindings: String, grants: String): String {
        val (engine, artifact) = live { checkNotNull(engine) to checkNotNull(artifacts[id]) }
        val handle = UUID.randomUUID().toString()
        val port = engine.open(artifact, bindings, grants)
        try { live { ports[handle] = port } }
        catch (error: Throwable) { port.close(); throw error }
        return handle
    }

    /** Snapshot a foreign owner before awaiting an asynchronous event. */
    fun port(id: String): NativePort = live { checkNotNull(ports[id]) { "Unknown revision" } }

    /** Read the validated executable metadata. */
    fun description(id: String): String = live { port(id).description() }

    /** Drain bounded host-attributed diagnostics. */
    fun logs(id: String): String = live { port(id).takeLogs() }

    /** Keep storage owned if an admitted callback prevents explicit close. */
    fun closePort(id: String) = live {
        ports[id]?.let { it.shutdown(); it.close(); ports.remove(id) }
        Unit
    }

    /** Release foreign owners on invalidation; physical work retains its native lifetime. */
    override fun close() {
        val pending = synchronized(this) {
            if (closed) return
            closed = true
            val pending = ports.values.toList() to artifacts.values.toList()
            ports.clear(); artifacts.clear()
            engine?.close(); engine = null
            pending
        }
        kotlin.concurrent.thread(isDaemon = true, name = "tembra-mobile-close") {
            pending.first.forEach { it.invalidate(); it.close() }
            pending.second.forEach { it.close() }
        }
    }
}
