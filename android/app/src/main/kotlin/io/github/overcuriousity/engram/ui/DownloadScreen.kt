package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.contained.Model

/**
 * What stands between choosing the phone and using it: the models contained
 * mode cannot open without. The way back to a server is here too, because a
 * download that cannot be made must not be a place with no exit.
 */
@Composable
fun DownloadScreen(engram: Engram, missing: List<Model>, onChanged: () -> Unit, onServer: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.CenterVertically)) {
        Wordmark()
        Text("On this phone", style = MaterialTheme.typography.titleLarge)
        missing.forEach { ModelLine(engram, it, onChanged) }
        TextButton(onClick = onServer) { Text("With a server instead") }
    }
}
