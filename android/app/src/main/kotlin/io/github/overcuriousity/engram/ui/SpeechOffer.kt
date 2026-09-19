package io.github.overcuriousity.engram.ui

import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable

/**
 * The first press of the microphone on a phone that has nothing to listen
 * with. Made once: "Not now" takes the button away, as it is against a server
 * with no speech model, and Settings' Models is the way back.
 */
@Composable
fun SpeechOfferDialog(model: @Composable () -> Unit, onNotNow: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Dictation · no model yet") },
        text = { model() },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
        dismissButton = { TextButton(onClick = onNotNow) { Text("Not now") } },
    )
}
