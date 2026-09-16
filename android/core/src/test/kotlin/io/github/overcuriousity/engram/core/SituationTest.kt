package io.github.overcuriousity.engram.core

import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.*
import org.junit.Test

class SituationTest {
    private val stable = Stable("Android", "engram-android", 1080, 2400, 8, 8.0, "de-DE")

    private fun src(
        tz: String = "Europe/Berlin", dark: Boolean = true, portrait: Boolean = true,
        battery: Float = 0.5f, charging: Boolean = false, network: String? = "wifi",
        route: String = "speaker", dnd: Boolean = false, place: String? = "u33dc0",
    ) = object : SituationSource {
        override val tz = tz; override val tzOffsetMins = 120
        override val dark = dark; override val portrait = portrait
        override val batteryLevel = battery; override val charging = charging
        override val powerSave = false; override val docked = false
        override val network = network; override val downlinkMbit = 50f; override val saveData = false
        override val audioRoute = route; override val headset = route != "speaker"
        override val dnd = dnd; override val ringer = "normal"
        override val brightness = 0.7f; override val lux = 120f
        override val dpr = 2.75f; override val languages = listOf("de-DE", "en-GB")
        override val hourCycle = "h23"; override val reducedMotion = false; override val highContrast = false
        override val videoInputs = 2; override val audioInputs = 1; override val audioOutputs = 1
        override val sinceLastViewS = 30f; override val viewsToday = 3
        override fun place() = place
    }

    @Test fun theStableHalfDoesNotMoveWithTheSituation() {
        val a = Situation(src(), stable).bundle(placeOn = true)
        val b = Situation(
            src(
                tz = "America/New_York", dark = false, portrait = false, battery = 0.1f,
                charging = true, network = "cellular", route = "car", dnd = true, place = "dr5reg",
            ),
            stable,
        ).bundle(placeOn = true)
        for (k in listOf("platform", "ua_family", "screen_w", "screen_h", "cores", "memory_gb", "language")) {
            assertEquals(k, a[k], b[k])
        }
        assertNotEquals(a["audio_route"], b["audio_route"])
    }

    @Test fun everyKeyIsInTheVocabulary() {
        val allowed = javaClass.getResource("/bundle-fields.txt")!!.readText().lines().filter { it.isNotBlank() }.toSet()
        val keys = Situation(src(), stable).bundle(placeOn = true).keys
        assertTrue("outside the vocabulary: ${keys - allowed}", allowed.containsAll(keys))
    }

    @Test fun placeIsAbsentWhenTheSwitchIsOff() {
        assertEquals(JsonNull, Situation(src(), stable).bundle(placeOn = false)["place"])
        assertEquals("u33dc0", Situation(src(), stable).bundle(placeOn = true)["place"]!!.jsonPrimitive.content)
    }

    @Test fun theAppSaysWhatItIs() {
        val b = Situation(src(), stable).bundle(placeOn = false)
        assertEquals("app", b["display_mode"]!!.jsonPrimitive.content)
        assertEquals("coarse", b["pointer"]!!.jsonPrimitive.content)
        assertEquals("true", b["touch"]!!.jsonPrimitive.content)
        assertEquals("portrait", b["orientation"]!!.jsonPrimitive.content)
        assertEquals("dark", b["color_scheme"]!!.jsonPrimitive.content)
    }

    @Test fun theGeohashIsTheStandardOne() {
        // Berlin, the same cell the browser test uses.
        assertEquals("u33dc0", Geohash.encode(52.52, 13.405, 6))
    }
}
