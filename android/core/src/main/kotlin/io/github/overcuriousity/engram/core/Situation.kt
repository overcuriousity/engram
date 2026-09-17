package io.github.overcuriousity.engram.core

import android.annotation.SuppressLint
import android.app.ActivityManager
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.SharedPreferences
import android.content.res.Configuration
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.hardware.camera2.CameraManager
import android.location.LocationManager
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.BatteryManager
import android.os.PowerManager
import android.provider.Settings
import android.text.format.DateFormat
import android.view.WindowManager
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import java.time.LocalDate
import java.time.ZoneId
import java.time.ZonedDateTime
import java.util.Locale
import kotlin.math.roundToInt

/**
 * The six fields `device_key` on the server hashes. Read once; a phone that
 * rotates, unplugs or moves is the same phone.
 */
data class Stable(
    val platform: String,
    val uaFamily: String,
    val screenW: Int,
    val screenH: Int,
    val cores: Int,
    val memoryGb: Double,
    val language: String,
) {
    companion object {
        fun of(context: Context): Stable {
            val wm = context.getSystemService(WindowManager::class.java)
            val b = wm.maximumWindowMetrics.bounds
            val w = minOf(b.width(), b.height())
            val h = maxOf(b.width(), b.height())
            val mi = ActivityManager.MemoryInfo().also { context.getSystemService(ActivityManager::class.java).getMemoryInfo(it) }
            val gb = (mi.totalMem / 1_073_741_824.0 * 2).roundToInt() / 2.0
            return Stable(
                "Android", "engram-android", w, h, Runtime.getRuntime().availableProcessors(), gb,
                Locale.getDefault().toLanguageTag(),
            )
        }
    }
}

/** Everything the platform is asked for each time. One property per reading, so a test can fake it. */
interface SituationSource {
    val tz: String
    val tzOffsetMins: Int
    val dark: Boolean
    val portrait: Boolean
    val batteryLevel: Float?
    val charging: Boolean
    val powerSave: Boolean
    val docked: Boolean
    val network: String?
    val downlinkMbit: Float?
    val saveData: Boolean
    val audioRoute: String
    val headset: Boolean
    val dnd: Boolean
    val ringer: String
    val brightness: Float?
    val lux: Float?
    val dpr: Float
    val languages: List<String>
    val hourCycle: String
    val reducedMotion: Boolean
    val highContrast: Boolean
    val videoInputs: Int
    val audioInputs: Int
    val audioOutputs: Int
    val sinceLastViewS: Float?
    val viewsToday: Int
    /** 6-character geohash from the last known passive position, or null. Only called when the switch is on. */
    fun place(): String?
}

/**
 * The bundle the server's `Bundle` reads, with the stable half fixed by
 * construction and the rest read from the platform each time it is asked.
 * Nothing in Part D posts it; Part E does.
 */
class Situation(private val src: SituationSource, private val stable: Stable) {
    fun bundle(placeOn: Boolean): JsonObject = buildJsonObject {
        // The nineteen the browser has always sent.
        put("tz", src.tz)
        put("tz_offset_mins", src.tzOffsetMins)
        put("language", stable.language)
        putJsonArray("languages") { src.languages.forEach { add(JsonPrimitive(it)) } }
        put("viewport_w", stable.screenW)
        put("viewport_h", stable.screenH)
        put("screen_w", stable.screenW)
        put("screen_h", stable.screenH)
        put("dpr", src.dpr)
        put("color_scheme", if (src.dark) "dark" else "light")
        put("platform", stable.platform)
        put("ua_family", stable.uaFamily)
        put("cores", stable.cores)
        put("memory_gb", stable.memoryGb)
        put("touch", true)
        put("orientation", if (src.portrait) "portrait" else "landscape")
        put("network", src.network)
        put("battery_level", src.batteryLevel)
        put("charging", src.charging)
        put("audio_outputs", src.audioOutputs)
        // Stored, not encoded — the wider vocabulary.
        put("place", if (placeOn) src.place() else null)
        put("net_effective", null as String?)
        put("net_downlink", src.downlinkMbit)
        put("rtt", null as Float?)
        put("save_data", src.saveData)
        put("reduced_motion", src.reducedMotion)
        put("high_contrast", src.highContrast)
        put("pointer", "coarse")
        put("hover", false)
        put("display_mode", "app")
        put("nav_type", null as String?)
        put("window_state", null as String?)
        put("focused", true)
        put("since_last_view_s", src.sinceLastViewS)
        put("views_today", src.viewsToday)
        put("screen_x", null as Float?)
        put("screen_y", null as Float?)
        put("screens", 1)
        put("avail_w", null as Float?)
        put("avail_h", null as Float?)
        put("video_inputs", src.videoInputs)
        put("audio_inputs", src.audioInputs)
        put("zoom", null as Float?)
        put("fullscreen", false)
        put("referrer_kind", null as String?)
        put("online", src.network != null)
        put("keyboard_layout", null as String?)
        put("hour_cycle", src.hourCycle)
        put("audio_route", src.audioRoute)
        put("dnd", src.dnd)
        put("ringer", src.ringer)
        put("power_save", src.powerSave)
        put("brightness", src.brightness)
        put("lux", src.lux)
        put("docked", src.docked)
        put("headset", src.headset)
    }
}

/** The real readings. Every one is a getter, so a bundle is the moment it is built. */
class AndroidSituationSource(private val context: Context, private val counters: ViewCounters) : SituationSource {
    private val audio get() = context.getSystemService(AudioManager::class.java)
    private val conn get() = context.getSystemService(ConnectivityManager::class.java)
    private val power get() = context.getSystemService(PowerManager::class.java)
    private val notif get() = context.getSystemService(NotificationManager::class.java)
    private val battery get() = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
    private val caps get() = conn.activeNetwork?.let { conn.getNetworkCapabilities(it) }

    override val tz get() = ZoneId.systemDefault().id
    override val tzOffsetMins get() = ZonedDateTime.now().offset.totalSeconds / 60
    override val dark get() = context.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES
    override val portrait get() = context.resources.configuration.orientation != Configuration.ORIENTATION_LANDSCAPE
    override val batteryLevel: Float? get() = battery?.let {
        val l = it.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val s = it.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
        if (l < 0 || s <= 0) null else l.toFloat() / s
    }
    override val charging get() = (battery?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0) != 0
    override val powerSave get() = power.isPowerSaveMode
    override val docked: Boolean get() {
        val plugged = battery?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0
        val dock = context.registerReceiver(null, IntentFilter(Intent.ACTION_DOCK_EVENT))
            ?.getIntExtra(Intent.EXTRA_DOCK_STATE, Intent.EXTRA_DOCK_STATE_UNDOCKED) ?: Intent.EXTRA_DOCK_STATE_UNDOCKED
        return plugged == BatteryManager.BATTERY_PLUGGED_WIRELESS || dock != Intent.EXTRA_DOCK_STATE_UNDOCKED
    }
    override val network: String? get() = caps?.let {
        when {
            it.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
            it.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
            it.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "wired"
            else -> "other"
        }
    }
    override val downlinkMbit: Float? get() = caps?.linkDownstreamBandwidthKbps?.takeIf { it > 0 }?.let { it / 1000f }
    override val saveData get() = conn.isActiveNetworkMetered && conn.restrictBackgroundStatus == ConnectivityManager.RESTRICT_BACKGROUND_STATUS_ENABLED
    override val audioRoute: String get() {
        val outs = audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
        return when {
            outs.any { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP && it.productName.contains("car", true) } -> "car"
            outs.any { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP || it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO } -> "bluetooth"
            outs.any { it.type == AudioDeviceInfo.TYPE_WIRED_HEADPHONES || it.type == AudioDeviceInfo.TYPE_WIRED_HEADSET || it.type == AudioDeviceInfo.TYPE_USB_HEADSET } -> "wired"
            else -> "speaker"
        }
    }
    override val headset get() = audioRoute == "wired" || audioRoute == "bluetooth"
    override val dnd get() = notif.currentInterruptionFilter != NotificationManager.INTERRUPTION_FILTER_ALL
    override val ringer get() = when (audio.ringerMode) {
        AudioManager.RINGER_MODE_SILENT -> "silent"
        AudioManager.RINGER_MODE_VIBRATE -> "vibrate"
        else -> "normal"
    }
    override val brightness: Float? get() = runCatching {
        Settings.System.getInt(context.contentResolver, Settings.System.SCREEN_BRIGHTNESS) / 255f
    }.getOrNull()
    override val lux: Float? get() = LightSample.last
    override val dpr get() = context.resources.displayMetrics.density
    override val languages: List<String> get() {
        val l = context.resources.configuration.locales
        return (0 until l.size()).map { l[it].toLanguageTag() }
    }
    override val hourCycle get() = if (DateFormat.is24HourFormat(context)) "h23" else "h12"
    override val reducedMotion get() = Settings.Global.getFloat(context.contentResolver, Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f
    override val highContrast get() = runCatching { Settings.Secure.getInt(context.contentResolver, "high_text_contrast_enabled") == 1 }.getOrDefault(false)
    override val videoInputs get() = runCatching { context.getSystemService(CameraManager::class.java).cameraIdList.size }.getOrDefault(0)
    override val audioInputs get() = audio.getDevices(AudioManager.GET_DEVICES_INPUTS).size
    override val audioOutputs get() = audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS).size
    override val sinceLastViewS get() = counters.sinceLastViewS()
    override val viewsToday get() = counters.viewsToday()

    // Only reached with the Place switch on, which cannot be turned on until
    // ACCESS_COARSE_LOCATION is granted — and a revoked grant lands in the
    // runCatching below rather than anywhere else.
    @SuppressLint("MissingPermission")
    override fun place(): String? {
        val lm = context.getSystemService(LocationManager::class.java)
        val loc = runCatching { lm.getLastKnownLocation(LocationManager.PASSIVE_PROVIDER) }.getOrNull() ?: return null
        return Geohash.encode(loc.latitude, loc.longitude, 6)
    }
}

/** The light sensor's last reading, kept by whoever registers it (the Activity while resumed). */
object LightSample : SensorEventListener {
    @Volatile var last: Float? = null

    fun start(context: Context) {
        val sm = context.getSystemService(SensorManager::class.java)
        sm.getDefaultSensor(Sensor.TYPE_LIGHT)?.let { sm.registerListener(this, it, SensorManager.SENSOR_DELAY_NORMAL) }
    }

    fun stop(context: Context) = context.getSystemService(SensorManager::class.java).unregisterListener(this)
    override fun onSensorChanged(e: SensorEvent) { last = e.values.firstOrNull() }
    override fun onAccuracyChanged(s: Sensor?, a: Int) {}
}

/** The same two counters the browser keeps in localStorage. */
class ViewCounters(private val prefs: SharedPreferences) {
    fun sinceLastViewS(): Float? =
        prefs.getLong("last_view", 0L).takeIf { it > 0 }?.let { (System.currentTimeMillis() - it) / 1000f }

    /** What `mark` has counted today. A reading, like `sinceLastViewS` beside it. */
    fun viewsToday(): Int {
        val day = LocalDate.now().toString()
        return if (prefs.getString("views_day", "") == day) prefs.getInt("views_n", 0) else 0
    }

    /** This view. The count the next bundle reports is the one this leaves behind. */
    fun mark() {
        val day = LocalDate.now().toString()
        prefs.edit()
            .putLong("last_view", System.currentTimeMillis())
            .putString("views_day", day)
            .putInt("views_n", viewsToday() + 1)
            .apply()
    }
}

/** Standard geohash, the same function as `geohash()` in app.js. */
object Geohash {
    private const val CHARS = "0123456789bcdefghjkmnpqrstuvwxyz"

    fun encode(lat: Double, lon: Double, precision: Int): String {
        val latR = doubleArrayOf(-90.0, 90.0)
        val lonR = doubleArrayOf(-180.0, 180.0)
        val out = StringBuilder()
        var bit = 0
        var ch = 0
        var even = true
        while (out.length < precision) {
            if (even) {
                val mid = (lonR[0] + lonR[1]) / 2
                if (lon >= mid) { ch = (ch shl 1) or 1; lonR[0] = mid } else { ch = ch shl 1; lonR[1] = mid }
            } else {
                val mid = (latR[0] + latR[1]) / 2
                if (lat >= mid) { ch = (ch shl 1) or 1; latR[0] = mid } else { ch = ch shl 1; latR[1] = mid }
            }
            even = !even
            if (++bit == 5) { out.append(CHARS[ch]); bit = 0; ch = 0 }
        }
        return out.toString()
    }
}
