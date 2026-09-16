package io.github.overcuriousity.engram.core.push

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull

data class Moment(val id: String, val title: String, val at: Long)

/** What the server's `jobs::webpush::Payload` becomes on the phone. A version this app does not know still rings. */
sealed class Payload {
    data class Due(val at: Long, val moments: List<Moment>, val more: Int) : Payload()
    data class Notice(val at: Long, val title: String, val body: String) : Payload()
    data class Unknown(val version: Int?) : Payload()

    companion object {
        const val VERSION = 1

        fun parse(bytes: ByteArray): Payload = runCatching {
            val o = Json.parseToJsonElement(String(bytes, Charsets.UTF_8)).jsonObject
            val v = o["v"]?.jsonPrimitive?.intOrNull
            if (v != VERSION) return Unknown(v)
            val at = o["at"]?.jsonPrimitive?.longOrNull ?: 0L
            when (o["kind"]?.jsonPrimitive?.content) {
                "due" -> Due(
                    at,
                    o["moments"]?.jsonArray?.map { m ->
                        val mo = m.jsonObject
                        Moment(
                            mo["id"]!!.jsonPrimitive.content,
                            mo["title"]?.jsonPrimitive?.content ?: "",
                            mo["at"]?.jsonPrimitive?.longOrNull ?: at,
                        )
                    } ?: emptyList(),
                    o["more"]?.jsonPrimitive?.intOrNull ?: 0,
                )
                "notice" -> Notice(at, o["title"]?.jsonPrimitive?.content ?: "", o["body"]?.jsonPrimitive?.content ?: "")
                else -> Unknown(null)
            }
        }.getOrDefault(Unknown(null))
    }
}
