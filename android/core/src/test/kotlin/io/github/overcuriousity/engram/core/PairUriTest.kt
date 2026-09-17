package io.github.overcuriousity.engram.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairUriTest {
    // Exactly what `pair_uri` in src/web/app.rs writes.
    private val server = "engram://pair?o=https%3A%2F%2Fengram.test&c=abc-DEF_123&v=0.1.0"

    @Test fun theServersOwnOutputParses() {
        val p = PairUri.parse(server)!!
        assertEquals("https://engram.test", p.origin)
        assertEquals("abc-DEF_123", p.code)
        assertEquals("0.1.0", p.serverVersion)
        assertNull(p.fingerprint)
    }

    @Test fun aFingerprintRidesAlong() {
        val f = "A".repeat(43)
        assertEquals(f, PairUri.parse("$server&f=$f")!!.fingerprint)
    }

    @Test fun aFingerprintOfTheWrongLengthIsRefused() {
        assertNull(PairUri.parse("$server&f=short"))
    }

    @Test fun aTrailingSlashOnTheOriginIsDropped() {
        assertEquals("https://engram.test", PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test%2F&c=x&v=1")!!.origin)
    }

    @Test fun plainHttpIsOnlyForLoopback() {
        assertNull(PairUri.parse("engram://pair?o=http%3A%2F%2Fengram.test&c=x&v=1"))
        assertEquals("http://127.0.0.1:8080", PairUri.parse("engram://pair?o=http%3A%2F%2F127.0.0.1%3A8080&c=x&v=1")!!.origin)
        assertEquals("http://localhost:8080", PairUri.parse("engram://pair?o=http%3A%2F%2Flocalhost%3A8080&c=x&v=1")!!.origin)
    }

    @Test fun anOriginWithAPathIsRefused() {
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test%2Fui&c=x&v=1"))
    }

    @Test fun anythingElseIsNull() {
        assertNull(PairUri.parse("https://engram.test/ui/app"))
        assertNull(PairUri.parse("engram://other?o=https%3A%2F%2Fengram.test&c=x&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&c=&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&c=x"))
        assertNull(PairUri.parse(""))
    }
}
