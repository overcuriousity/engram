package io.github.overcuriousity.engram.doors

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The WAV the microphone hands over: the shape `toWav` in app.js writes and
 * whisper's `dr_wav` reads without an argument. Every field is checked by
 * offset, because a reader that disagrees about one of them hears noise.
 */
class MicrophoneTest {
    @Test fun theHeaderIsARiffMono16BitPcmWrapperAndTheSamplesFollowIt() {
        val pcm = byteArrayOf(1, 0, 2, 0, 3, 0)
        val w = wav(pcm, 16_000)
        assertEquals(44 + pcm.size, w.size)
        val b = ByteBuffer.wrap(w).order(ByteOrder.LITTLE_ENDIAN)
        assertEquals("RIFF", String(w, 0, 4, Charsets.US_ASCII))
        assertEquals(36 + pcm.size, b.getInt(4))
        assertEquals("WAVE", String(w, 8, 4, Charsets.US_ASCII))
        assertEquals("fmt ", String(w, 12, 4, Charsets.US_ASCII))
        assertEquals(16, b.getInt(16))
        assertEquals(1, b.getShort(20).toInt())        // PCM
        assertEquals(1, b.getShort(22).toInt())        // mono
        assertEquals(16_000, b.getInt(24))
        assertEquals(32_000, b.getInt(28))             // bytes per second
        assertEquals(2, b.getShort(32).toInt())        // bytes per frame
        assertEquals(16, b.getShort(34).toInt())       // bits per sample
        assertEquals("data", String(w, 36, 4, Charsets.US_ASCII))
        assertEquals(pcm.size, b.getInt(40))
        assertArrayEquals(pcm, w.copyOfRange(44, w.size))
    }

    /** A press and a release with nothing between: a header and no samples, which the screen drops. */
    @Test fun nothingHeardIsAHeaderAlone() {
        assertEquals(Microphone.HEADER, wav(ByteArray(0), Microphone.RATE).size)
    }
}
