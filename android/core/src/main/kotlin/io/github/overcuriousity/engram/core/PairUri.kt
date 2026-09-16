package io.github.overcuriousity.engram.core

import java.net.URI
import java.net.URLDecoder

/**
 * What the QR on `/ui/app` carries. Parsed strictly: a scanner hands over
 * whatever it saw, and the one thing this must never do is claim against an
 * origin the server did not name.
 */
data class PairUri(
    val origin: String,
    val code: String,
    val serverVersion: String,
    val fingerprint: String?,
) {
    companion object {
        private val B64URL_43 = Regex("^[A-Za-z0-9_-]{43}$")

        fun parse(text: String): PairUri? {
            val t = text.trim()
            if (!t.startsWith("engram://pair?")) return null
            val q = t.removePrefix("engram://pair?")
                .split('&')
                .mapNotNull { kv ->
                    val i = kv.indexOf('=')
                    if (i < 0) null else kv.substring(0, i) to URLDecoder.decode(kv.substring(i + 1), "UTF-8")
                }
                .toMap()
            val code = q["c"]?.takeIf { it.isNotEmpty() } ?: return null
            val version = q["v"]?.takeIf { it.isNotEmpty() } ?: return null
            val origin = normaliseOrigin(q["o"] ?: return null) ?: return null
            val f = q["f"]
            if (f != null && !B64URL_43.matches(f)) return null
            return PairUri(origin, code, version, f)
        }

        /** `scheme://host[:port]`, and nothing else. */
        private fun normaliseOrigin(raw: String): String? {
            val u = runCatching { URI(raw) }.getOrNull() ?: return null
            val host = u.host ?: return null
            if (u.userInfo != null || u.query != null || u.fragment != null) return null
            if (u.rawPath != "" && u.rawPath != "/") return null
            val loopback = host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]"
            when (u.scheme) {
                "https" -> {}
                "http" -> if (!loopback) return null
                else -> return null
            }
            val port = if (u.port == -1) "" else ":${u.port}"
            return "${u.scheme}://$host$port"
        }
    }
}
