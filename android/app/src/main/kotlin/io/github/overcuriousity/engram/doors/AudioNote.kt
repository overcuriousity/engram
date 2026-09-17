package io.github.overcuriousity.engram.doors

import android.content.Context
import android.media.MediaRecorder
import android.os.Build
import java.io.File

/** The microphone door: one recording at a time, to an .m4a the server transcribes. */
class AudioNote(private val context: Context) {
    private var recorder: MediaRecorder? = null
    var file: File? = null
        private set

    fun start(): File {
        val f = File.createTempFile("note", ".m4a", context.cacheDir)
        // The constructor taking a Context is API 31; before it there is only
        // the bare one, deprecated since and the same recorder.
        val r = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            MediaRecorder(context)
        } else {
            @Suppress("DEPRECATION")
            MediaRecorder()
        }
        recorder = r.apply {
            setAudioSource(MediaRecorder.AudioSource.MIC)
            setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            setAudioEncodingBitRate(64_000)
            setAudioSamplingRate(44_100)
            setOutputFile(f.path)
            prepare()
            start()
        }
        file = f
        return f
    }

    fun stop(): File? {
        runCatching { recorder?.stop() }
        recorder?.release()
        recorder = null
        return file
    }
}
