package io.github.overcuriousity.engram.core

import org.junit.Assert.assertTrue
import org.junit.Test

class ScaffoldTest {
    @Test fun theFixtureIsOnTheClasspath() {
        val text = javaClass.getResource("/bundle-fields.txt")!!.readText()
        assertTrue(text.lines().contains("tz"))
    }
}
