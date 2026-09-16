package io.github.overcuriousity.engram.doors

import android.app.PendingIntent
import android.content.Intent
import android.service.quicksettings.TileService
import io.github.overcuriousity.engram.MainActivity

/** The quick-settings tile: the empty composer, one pull-down away. */
class CaptureTile : TileService() {
    override fun onClick() {
        val i = Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivityAndCollapse(PendingIntent.getActivity(this, 0, i, PendingIntent.FLAG_IMMUTABLE))
    }
}
