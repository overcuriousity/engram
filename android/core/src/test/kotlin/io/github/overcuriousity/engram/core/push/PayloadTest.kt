package io.github.overcuriousity.engram.core.push

import org.junit.Assert.*
import org.junit.Test

class PayloadTest {
    @Test fun aVersionOneDueParses() {
        val p = Payload.parse("""{"v":1,"kind":"due","at":1700000000,"moments":[{"id":"m1","title":"Call","at":1700000100}],"more":2}""".toByteArray())
        val d = p as Payload.Due
        assertEquals(1700000000L, d.at); assertEquals(2, d.more)
        assertEquals(Moment("m1", "Call", 1700000100L), d.moments.single())
    }

    @Test fun aNoticeParses() {
        val n = Payload.parse("""{"v":1,"kind":"notice","at":1,"title":"Test","body":"It works"}""".toByteArray()) as Payload.Notice
        assertEquals("Test", n.title); assertEquals("It works", n.body)
    }

    @Test fun aLaterVersionStillRings() {
        assertEquals(Payload.Unknown(2), Payload.parse("""{"v":2,"kind":"due","at":1}""".toByteArray()))
    }

    @Test fun garbageStillRings() {
        assertEquals(Payload.Unknown(null), Payload.parse(byteArrayOf(0, 1, 2)))
        assertEquals(Payload.Unknown(null), Payload.parse("""{"v":1,"kind":"other"}""".toByteArray()))
    }
}
