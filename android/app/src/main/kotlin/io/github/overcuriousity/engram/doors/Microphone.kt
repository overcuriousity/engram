package io.github.overcuriousity.engram.doors

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import androidx.core.content.ContextCompat
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The microphone door: held open while a button is, and what it heard comes
 * back as one WAV. There used to be a recorder here that wrote an .m4a for
 * the capture queue to send as a file. That was a different thing — a voice
 * note, kept — and not what the web's button does. The web's button dictates:
 * what is said is typed into the box, and pressing something is still what
 * acts on it. This is that.
 *
 * 16 kHz mono 16-bit PCM in a RIFF wrapper, exactly what the web converts its
 * recording to before sending (see `toWav` in app.js), and for the same
 * reason: it is what whisper resamples to anyway, and a whisper built without
 * ffmpeg reads WAV and nothing else. The phone can record it directly, so
 * there is no conversion.
 */
class Microphone(private val context: Context) {
    private var reader: Thread? = null
    private val pcm = ByteArrayOutputStream()
    @Volatile private var running = false

    val allowed: Boolean
        get() = ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    /** Open the door. False where the permission is not held or no microphone answers. */
    // The permission is checked through `allowed` on the first line, which
    // lint cannot see through.
    @SuppressLint("MissingPermission")
    fun start(): Boolean {
        if (running || !allowed) return false
        val min = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
        if (min <= 0) return false
        val r = try {
            AudioRecord(MediaRecorder.AudioSource.MIC, RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, maxOf(min, RATE))
        } catch (e: Exception) {
            return false
        }
        if (r.state != AudioRecord.STATE_INITIALIZED) { r.release(); return false }
        pcm.reset()
        running = true
        r.startRecording()
        // The reader owns the recorder and lets it go as it leaves. Nobody
        // else may: a release from `stop` is a release of the native object
        // this thread is sitting inside `read` on.
        reader = Thread({
            val buf = ByteArray(4096)
            try {
                while (running) {
                    val n = r.read(buf, 0, buf.size)
                    if (n > 0) pcm.write(buf, 0, n) else if (n < 0) break
                }
            } finally {
                runCatching { r.stop() }
                r.release()
            }
        }, "engram-mic").also { it.start() }
        return true
    }

    /** Close the door: what was heard, as a WAV. Empty samples for a press and a release with nothing between. */
    fun stop(): ByteArray {
        if (!running) return wav(ByteArray(0), RATE)
        running = false
        // Bounded, because this is a press being released and the hand must
        // not be held: what the join is for is the samples, not the recorder,
        // and a join that times out costs the last buffer rather than a
        // recorder pulled out from under the thread still reading it.
        reader?.join(1000)
        reader = null
        return wav(pcm.toByteArray(), RATE)
    }

    companion object {
        const val RATE = 16_000
        const val MIME = "audio/wav"
        /** The RIFF header alone: a recording of this length holds no samples. */
        const val HEADER = 44
    }
}

/**
 * A RIFF header and the samples: the shape every minimal WAV reader accepts
 * without an argument — `dr_wav`, which is whisper's, among them. Mono, 16-bit,
 * no extensible subformat, every length known before a byte is written.
 */
fun wav(pcm: ByteArray, rate: Int): ByteArray {
    val b = ByteBuffer.allocate(44 + pcm.size).order(ByteOrder.LITTLE_ENDIAN)
    b.put("RIFF".toByteArray(Charsets.US_ASCII))
    b.putInt(36 + pcm.size)
    b.put("WAVE".toByteArray(Charsets.US_ASCII))
    b.put("fmt ".toByteArray(Charsets.US_ASCII))
    b.putInt(16)                   // the fmt chunk's own length
    b.putShort(1)                  // PCM, uncompressed
    b.putShort(1)                  // mono
    b.putInt(rate)
    b.putInt(rate * 2)             // bytes per second
    b.putShort(2)                  // bytes per frame
    b.putShort(16)                 // bits per sample
    b.put("data".toByteArray(Charsets.US_ASCII))
    b.putInt(pcm.size)
    b.put(pcm)
    return b.array()
}
