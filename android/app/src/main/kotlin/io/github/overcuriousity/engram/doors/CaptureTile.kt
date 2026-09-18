package io.github.overcuriousity.engram.doors

import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.service.quicksettings.TileService
import io.github.overcuriousity.engram.MainActivity

/** The quick-settings tile: the empty composer, one pull-down away. */
class CaptureTile : TileService() {
    override fun onClick() {
        val i = Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK).putExtra("screen", "capture")
        // The PendingIntent form is API 34, and from 34 on the Intent form
        // throws — so each side of the line gets the one it has.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, i, PendingIntent.FLAG_IMMUTABLE))
        } else {
            @Suppress("DEPRECATION", "StartActivityAndCollapseDeprecated")
            startActivityAndCollapse(i)
        }
    }
}
