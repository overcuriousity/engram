package io.github.overcuriousity.engram.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.IOException
import java.security.KeyStore
import java.security.MessageDigest
import java.security.cert.X509Certificate
import java.util.Base64
import javax.net.ssl.TrustManagerFactory
import javax.net.ssl.X509TrustManager

/** The code was unknown, expired or already used. The server says no more. */
class ClaimRefused : IOException("this code has expired or was already used")

object Pairing {
    /**
     * Trade the scanned code for the real token. The one request made without
     * a bearer. When the QR carried no fingerprint, the leaf certificate the
     * handshake actually served is recorded as the pin — unless the chain was
     * publicly trusted, in which case there is nothing to pin and `pin` stays
     * null. A fingerprint in the QR is the pin from the first byte.
     */
    suspend fun claim(uri: PairUri, deviceName: String, userAgent: String): Connection = withContext(Dispatchers.IO) {
        val client = baseClient(userAgent, uri.fingerprint, java.net.URI(uri.origin).host)
        val body = """{"code":${js(uri.code)},"device":${js(deviceName)}}"""
        val req = Request.Builder()
            .url("${uri.origin}/api/v1/pair/claim")
            .post(body.toRequestBody("application/json".toMediaType()))
            .build()
        client.newCall(req).execute().use { res ->
            if (res.code == 401) throw ClaimRefused()
            val text = res.body.string()
            if (res.code != 201) throw IOException("claim: ${res.code} $text")
            val obj = Json.parseToJsonElement(text).jsonObject
            val token = obj["token"]!!.jsonPrimitive.content
            val version = obj["version"]?.jsonPrimitive?.content ?: uri.serverVersion
            val pin = uri.fingerprint
                ?: tofuPin(res.handshake?.peerCertificates.orEmpty().filterIsInstance<X509Certificate>())
            Connection(uri.origin, token, pin, version, deviceName)
        }
    }

    /** SPKI SHA-256, base64url unpadded — the same string `f=` would carry. */
    internal fun spki(cert: X509Certificate): String =
        Base64.getUrlEncoder().withoutPadding()
            .encodeToString(MessageDigest.getInstance("SHA-256").digest(cert.publicKey.encoded))

    private fun tofuPin(chain: List<X509Certificate>): String? {
        val leaf = chain.firstOrNull() ?: return null
        // A chain the system trusts needs no pin: the CA is the guarantee, and
        // a pin would only break the day the operator renews.
        return if (publiclyTrusted(chain)) null else spki(leaf)
    }

    /**
     * The whole chain the handshake served, under the authentication type the
     * leaf's key actually implies.
     *
     * Checking the leaf alone was not the safe direction to err in: no real CA
     * issues from its root directly, so path building failed for every
     * publicly-trusted deployment and every one of them was pinned by mistake
     * — and then broke on the operator's next renewal, with nothing on the
     * mismatch screen but "Unpair". Hardcoding "RSA" failed the same way for
     * an ECDSA leaf, which is what Let's Encrypt issues by default.
     */
    private fun publiclyTrusted(chain: List<X509Certificate>): Boolean = runCatching {
        val tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())
        tmf.init(null as KeyStore?)
        val tm = tmf.trustManagers.filterIsInstance<X509TrustManager>().first()
        tm.checkServerTrusted(chain.toTypedArray(), authType(chain.first()))
        true
    }.getOrDefault(false)

    /** What the key exchange would have been called. Every modern suite is ephemeral DH. */
    private fun authType(leaf: X509Certificate) =
        if (leaf.publicKey.algorithm == "EC") "ECDHE_ECDSA" else "ECDHE_RSA"

    private fun js(s: String) = Json.encodeToString(String.serializer(), s)
}
