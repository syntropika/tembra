package com.tembra

import com.facebook.react.bridge.ArrayBuffer
import com.tembra.ffi.MobileField
import com.tembra.ffi.MobileIngressEvent
import com.tembra.ffi.MobileRequestChunk
import com.tembra.ffi.MobileRequestHead
import org.json.JSONArray
import org.json.JSONObject

/** Translate between typed UniFFI ingress values and bridge metadata plus one byte buffer. */
internal object IngressCodec {
    /** Copy bytes borrowed from JavaScript before the synchronous bridge call returns. */
    fun copy(buffer: ArrayBuffer): ByteArray {
        val bytes = buffer.bytes.duplicate()
        bytes.clear()
        return ByteArray(bytes.remaining()).also { bytes.get(it) }
    }

    /** Names and lengths for metadata; values concatenated in order, duplicates kept. */
    private fun pack(fields: List<MobileField>): Pair<JSONArray, ArrayBuffer> {
        val buffer = ArrayBuffer(fields.sumOf { it.value.size })
        val bytes = buffer.bytes.duplicate()
        bytes.clear()
        val names = JSONArray()
        for (field in fields) {
            bytes.put(field.value)
            names.put(JSONArray().put(field.name).put(field.value.size))
        }
        return names to buffer
    }

    private fun string(value: JSONObject, key: String): String? =
        if (!value.has(key) || value.isNull(key)) null
        else value.get(key) as? String ?: error("Invalid ingress $key")

    /** Split one buffer by the metadata's names and lengths, which must cover it exactly. */
    fun fields(metadata: Any?, bytes: ByteArray): List<MobileField> {
        val entries = metadata as? JSONArray ?: error("Invalid ingress fields")
        var offset = 0
        val fields = (0 until entries.length()).map { index ->
            val entry = entries.get(index) as? JSONArray ?: error("Invalid ingress field")
            val name = entry.opt(0) as? String
            val length = (entry.opt(1) as? Number)?.toLong()
            check(entry.length() == 2 && name != null && length != null && length >= 0 && length <= bytes.size - offset) {
                "Invalid ingress field"
            }
            MobileField(name, bytes.copyOfRange(offset, offset + length.toInt())).also { offset += length.toInt() }
        }
        check(offset == bytes.size) { "Invalid ingress field lengths" }
        return fields
    }

    fun head(text: String, bytes: ByteArray): MobileRequestHead {
        val head = JSONObject(text)
        return MobileRequestHead(
            checkNotNull(string(head, "method")) { "Invalid ingress method" },
            string(head, "scheme"),
            string(head, "authority"),
            string(head, "pathWithQuery"),
            fields(head.opt("fields"), bytes),
        )
    }

    fun chunk(text: String, bytes: ByteArray): MobileRequestChunk {
        val chunk = JSONObject(text)
        return when (string(chunk, "kind")) {
            "data" -> MobileRequestChunk.Data(bytes)
            "end" -> MobileRequestChunk.End(fields(chunk.opt("trailers"), bytes))
            "error" -> {
                check(bytes.isEmpty()) { "Invalid ingress error chunk" }
                MobileRequestChunk.Error(checkNotNull(string(chunk, "message")) { "Invalid ingress error chunk" })
            }
            else -> error("Invalid ingress chunk")
        }
    }

    /** Metadata for JavaScript and, for byte events, the one buffer it must take. */
    fun event(event: MobileIngressEvent): Pair<String, ArrayBuffer?> = when (event) {
        is MobileIngressEvent.Host -> JSONObject().put("kind", "host").put("id", event.id.toLong())
            .put("asynchronous", event.asynchronous).put("packet", event.packet).toString() to null
        is MobileIngressEvent.Complete -> JSONObject().put("kind", "complete").put("result", event.result).toString() to null
        is MobileIngressEvent.Head -> pack(event.fields).let { (names, bytes) ->
            JSONObject().put("kind", "head").put("status", event.status.toInt()).put("fields", names).toString() to bytes
        }
        is MobileIngressEvent.RequestPull -> """{"kind":"requestPull"}""" to null
        is MobileIngressEvent.Data -> JSONObject().put("kind", "data").put("length", event.bytes.size).toString() to
            ArrayBuffer.arrayBufferWithCopiedBytes(event.bytes)
        is MobileIngressEvent.Trailers -> pack(event.fields).let { (names, bytes) ->
            JSONObject().put("kind", "trailers").put("fields", names).toString() to bytes
        }
        is MobileIngressEvent.End -> """{"kind":"end"}""" to null
        is MobileIngressEvent.Error -> JSONObject().put("kind", "error").put("message", event.message).toString() to null
        is MobileIngressEvent.Refused -> JSONObject().put("kind", "refused").put("length", event.code.size).toString() to
            ArrayBuffer.arrayBufferWithCopiedBytes(event.code)
    }
}
