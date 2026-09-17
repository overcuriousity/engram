package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Reach
import io.github.overcuriousity.engram.core.read.Read
import io.github.overcuriousity.engram.core.read.Request
import java.time.ZoneId

/** A read as a screen holds it: where it stands, and a way to ask again. */
class ReadState<T>(val read: Read<T>, val retry: () -> Unit)

/**
 * Ask the reader, and keep asking when told to. The screen learns a value,
 * when it was fetched and whether the source answered — never where it lives.
 */
@Composable
fun <T> rememberRead(engram: Engram, request: Request, decode: (String) -> T): ReadState<T> {
    var attempt by remember(request.key) { mutableIntStateOf(0) }
    var read by remember(request.key) { mutableStateOf(Read<T>(null, null, Reach.Fresh, loading = true)) }
    LaunchedEffect(request.key, attempt) {
        engram.reader.read(request, decode).collect { read = it }
    }
    return ReadState(read) { attempt++ }
}

/**
 * The frame every reading screen sits in. It says, plainly, the two things a
 * person must not have to guess: that the server could not be reached, and
 * that what they are looking at is from earlier. Nothing is fetched behind
 * their back, so what is shown without the server is what they opened before.
 */
@Composable
fun <T> ReadFrame(
    state: ReadState<T>,
    modifier: Modifier = Modifier,
    content: @Composable (T) -> Unit,
) {
    val r = state.read
    Column(modifier) {
        if (r.loading) LinearProgressIndicator(Modifier.fillMaxWidth())
        if (r.reach == Reach.Unreachable) Unreachable(r.fetchedAt, state.retry)
        r.error?.let { Text(it, Modifier.padding(16.dp, 8.dp), color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        r.value?.let { content(it) }
    }
}

@Composable
fun Unreachable(fetchedAt: Long?, onRetry: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = Modifier.fillMaxWidth()) {
        Row(
            Modifier.padding(start = 16.dp, end = 4.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            val when_ = fetchedAt?.let { " · " + fetchedWords(it, System.currentTimeMillis(), ZoneId.systemDefault()) } ?: ""
            Text("Server unreachable$when_", style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = onRetry) { Text("Retry") }
        }
    }
}

/**
 * A row's label. A name somebody gave is set as a name; the opening of a text
 * standing in for one is set as text — lighter, never where a name would go —
 * because it is not a name and must not read as one.
 */
@Composable
fun Label(text: String, named: Boolean, modifier: Modifier = Modifier, maxLines: Int = 2) {
    Text(
        text,
        modifier,
        maxLines = maxLines,
        overflow = TextOverflow.Ellipsis,
        style = MaterialTheme.typography.bodyLarge,
        fontWeight = if (named) FontWeight.Medium else FontWeight.Normal,
        color = if (named) MaterialTheme.colorScheme.onBackground else MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
fun SectionHead(text: String) {
    Text(
        text,
        Modifier.padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 4.dp),
        style = MaterialTheme.typography.labelLarge,
        color = muted(),
    )
}

/** A tappable line: a label, and a few quiet words to its right. */
@Composable
fun LinkRow(label: String, named: Boolean, trailing: String = "", onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(16.dp, 10.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Label(label, named, Modifier.weight(1f), maxLines = 1)
        if (trailing.isNotEmpty()) Text(trailing, Modifier.padding(start = 12.dp), style = MaterialTheme.typography.labelMedium, color = muted())
    }
}
