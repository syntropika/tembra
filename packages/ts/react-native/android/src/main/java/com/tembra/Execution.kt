package com.tembra

import com.tembra.ffi.NativeArtifact
import com.tembra.ffi.NativeComponent
import com.tembra.ffi.NativeInterpreter
import com.tembra.ffi.NativePort
import java.util.UUID

/** Own opaque execution identities for one host, independently of foreign callback threads. */
internal class Execution : AutoCloseable {
    private var closed = false
    private var engine: NativeInterpreter? = null
    private val artifacts = mutableMapOf<String, NativeArtifact>()
    private val sources = mutableMapOf<String, NativePort>()
    private val components = mutableMapOf<String, NativeComponent>()

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

    /** Create a new isolated Store and retain it only while its host remains live. */
    fun open(id: String, bindings: String, grants: String, component: Boolean): String {
        val (engine, artifact) = live { checkNotNull(engine) to checkNotNull(artifacts[id]) }
        val handle = UUID.randomUUID().toString()
        if (component) {
            val port = engine.openComponent(artifact, bindings, grants)
            try { live { components[handle] = port } }
            catch (error: Throwable) { port.close(); throw error }
        } else {
            val port = engine.open(artifact, bindings, grants)
            try { live { sources[handle] = port } }
            catch (error: Throwable) { port.close(); throw error }
        }
        return handle
    }

    /** Snapshot a foreign owner before awaiting an asynchronous event. */
    fun source(id: String): NativePort = live { checkNotNull(sources[id]) { "Unknown source revision" } }

    /** Resolve a compiled revision from this host only. */
    fun component(id: String): NativeComponent = live { checkNotNull(components[id]) { "Unknown component revision" } }

    /** Read the validated executable metadata for either implementation. */
    fun description(id: String): String = live {
        sources[id]?.description() ?: components[id]?.description() ?: error("Unknown revision")
    }

    /** Drain bounded host-attributed diagnostics for either implementation. */
    fun logs(id: String): String = live { sources[id]?.takeLogs() ?: components[id]?.takeLogs() ?: error("Unknown revision") }

    /** Keep source storage owned if an admitted callback prevents explicit close. */
    fun closePort(id: String) = live {
        sources[id]?.let { it.shutdown(); it.close(); sources.remove(id) }
        components[id]?.let { it.shutdown(); it.close(); components.remove(id) }
        Unit
    }

    /** Release foreign owners on invalidation; physical work retains its native lifetime. */
    override fun close() {
        val pending = synchronized(this) {
            if (closed) return
            closed = true
            val pending = Triple(sources.values.toList(), components.values.toList(), artifacts.values.toList())
            sources.clear(); components.clear(); artifacts.clear()
            engine?.close(); engine = null
            pending
        }
        kotlin.concurrent.thread(isDaemon = true, name = "tembra-mobile-close") {
            pending.first.forEach { it.invalidate(); it.close() }
            pending.second.forEach { it.shutdown(); it.close() }
            pending.third.forEach { it.close() }
        }
    }
}
