package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.R

/** Where the microphone stands: open while held, then busy while the words come back. */
data class MicState(val listening: Boolean = false, val busy: Boolean = false, val said: String = "")

/**
 * Hold to dictate, as the web's button and every messenger's: the door is
 * open for exactly as long as the finger is down. A press that drifts off the
 * button still ends the recording where the finger comes up — the gesture is
 * awaited to its release, wherever that is, and a cancelled one is a release.
 *
 * Nothing here records. What the hold means is the screen's to say; this only
 * says when it started and when it ended.
 */
@Composable
fun MicButton(state: MicState, onDown: () -> Unit, onUp: () -> Unit, modifier: Modifier = Modifier) {
    val tint = if (state.listening) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurfaceVariant
    Box(
        modifier
            .size(40.dp)
            .alpha(if (state.busy) 0.5f else 1f)
            .background(if (state.listening) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceContainer, CircleShape)
            .semantics { role = Role.Button; contentDescription = "Hold to dictate" }
            .pointerInput(state.busy) {
                if (state.busy) return@pointerInput
                detectTapGestures(onPress = {
                    onDown()
                    tryAwaitRelease()
                    onUp()
                })
            },
        contentAlignment = Alignment.Center,
    ) {
        Icon(painterResource(R.drawable.ic_mic), contentDescription = null, tint = tint, modifier = Modifier.size(20.dp))
    }
}
