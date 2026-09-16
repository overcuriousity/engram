package io.github.overcuriousity.engram.ui

import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import io.github.overcuriousity.engram.core.Engram

@Composable
fun SettingsScreen(engram: Engram, onUnpair: () -> Unit) {
    Text("Settings")
}
