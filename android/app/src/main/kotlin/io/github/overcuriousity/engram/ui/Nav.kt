package io.github.overcuriousity.engram.ui

import android.net.Uri
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import io.github.overcuriousity.engram.R
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.PinMismatch
import java.time.LocalDate
import java.time.ZoneId

sealed class Screen(val route: String, val label: String) {
    object Pair : Screen("pair", "Pair")
    object Search : Screen("search", "Search")
    object Today : Screen("today", "Today")
    object Library : Screen("library", "Library")
    object Queue : Screen("queue", "Queue")
    object Settings : Screen("settings", "Settings")

    // Reached from Settings and from nowhere else. Never in `bar`, never in
    // the top bar: judging is a mechanic to work towards removing, and a place
    // for it on the screen the app opens on would build a habit around it.
    object Pairs : Screen("judge/pairs", "Duplicate pairs")
    object Gaps : Screen("judge/gaps", "Gaps")
    object Journal : Screen("judge/journal", "While you were away")
}

/**
 * The places the bar holds. Home is the box, and the box is where capture
 * happens too — there is no Capture place, because there is no second box to
 * go to. The queue and the settings are looked at, not lived in, and sit above.
 */
internal val BAR = listOf(Screen.Search, Screen.Today, Screen.Library)

/** Where a row leads. Ids and dates travel in the route; nothing else does. */
private object Routes {
    const val ARTIFACT = "artifact/{id}"
    const val CORPUS = "corpus/{id}"
    const val DAY = "day/{date}"
    const val ASK = "ask?q={q}"
    fun artifact(id: String) = "artifact/${Uri.encode(id)}"
    fun corpus(id: String) = "corpus/${Uri.encode(id)}"
    fun day(date: LocalDate) = "day/$date"
    fun ask(q: String) = "ask?q=${Uri.encode(q)}"
}

/** The mark from logo.svg beside the name in Inter: wordmark.svg, without the SVG's text element. */
@Composable
fun Wordmark() {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Image(
            painterResource(R.drawable.mark), contentDescription = null, modifier = Modifier.size(28.dp),
            colorFilter = ColorFilter.tint(MaterialTheme.colorScheme.onBackground),
        )
        Spacer(Modifier.width(6.dp))
        Text("engram", fontFamily = Inter, fontWeight = FontWeight.Medium, fontSize = 22.sp, letterSpacing = (-0.3).sp)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EngramApp(
    engram: Engram,
    start: Screen? = null,
    pairText: String? = null,
    focusBox: Boolean = false,
    onUnpair: () -> Unit,
) {
    val connection by engram.store.current.collectAsStateWithLifecycle()
    val refused by engram.refused.collectAsStateWithLifecycle()
    val pinned by engram.pinMismatch.collectAsStateWithLifecycle()
    val nav = rememberNavController()

    if (pinned != null) {
        PinMismatchScreen(pinned!!, onUnpair = onUnpair)
        return
    }
    if (connection == null) {
        PairScreen(engram, initialText = pairText)
        return
    }

    val owed by engram.outbox.rows.collectAsStateWithLifecycle(emptyList())
    LaunchedEffect(Unit) { engram.prune() }

    val bar = BAR
    fun go(route: String) = nav.navigate(route) { launchSingleTop = true }

    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = { Wordmark() },
                actions = {
                    // The queue is worth a glance exactly when something is
                    // still owed or was refused, and it is only there then.
                    // The web has no queue at all; a phone has one because
                    // it is offline sometimes, and a mechanism that exists
                    // for the offline case has no business on the bar while
                    // everything has gone through. Settings keeps the way in.
                    if (queueWorthAGlance(owed)) {
                        val n = queuedCount(owed)
                        TextButton(onClick = { go(Screen.Queue.route) }) { Text(if (n > 0) "Queue $n" else "Queue") }
                    }
                    TextButton(onClick = { go(Screen.Settings.route) }) { Text("Settings") }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.background),
            )
        },
        bottomBar = {
            NavigationBar(containerColor = MaterialTheme.colorScheme.surface) {
                val current = nav.currentBackStackEntryAsState().value?.destination?.route
                bar.forEach { s ->
                    NavigationBarItem(
                        selected = current == s.route,
                        onClick = { nav.navigate(s.route) { launchSingleTop = true; popUpTo(Screen.Search.route) } },
                        icon = {},
                        label = { Text(s.label) },
                    )
                }
            }
        },
    ) { pad ->
        Column(Modifier.padding(pad)) {
            if (refused) RefusedBanner(onRescan = { nav.navigate(Screen.Pair.route) })
            // A code scanned while already paired is how a phone moves to
            // another server, and how it recovers from `refused`. It used to
            // land here on Capture with the code dropped: the Pair screen was
            // neither the start nor given the text.
            val startAt = start ?: if (pairText != null) Screen.Pair else Screen.Search
            val onArtifact: (String) -> Unit = { go(Routes.artifact(it)) }
            val onCorpus: (String) -> Unit = { go(Routes.corpus(it)) }
            NavHost(nav, startDestination = startAt.route) {
                composable(Screen.Search.route) { SearchScreen(engram, onArtifact, onAsk = { go(Routes.ask(it)) }, focusBox = focusBox) }
                composable(Screen.Today.route) {
                    DayScreen(engram, LocalDate.now(ZoneId.systemDefault()), onDay = { go(Routes.day(it)) }, onCorpus, onArtifact)
                }
                composable(Screen.Library.route) { LibraryScreen(engram, onCorpus) }
                composable(Screen.Queue.route) { QueueScreen(engram) }
                composable(Screen.Settings.route) {
                    SettingsScreen(engram, onJudging = { go(it.route) }, onQueue = { go(Screen.Queue.route) }, onUnpair = onUnpair)
                }
                composable(Screen.Pairs.route) { PairReviewScreen(engram, onArtifact) }
                composable(Screen.Gaps.route) { GapsScreen(engram) }
                composable(Screen.Journal.route) { JournalScreen(engram, onArtifact, onCorpus) }
                composable(Screen.Pair.route) { PairScreen(engram, initialText = pairText, knownOrigin = connection?.origin) }
                composable(Routes.ASK) { AskScreen(engram, it.arguments?.getString("q").orEmpty(), onArtifact) }
                composable(Routes.ARTIFACT) { ArtifactScreen(engram, it.arguments?.getString("id").orEmpty(), onCorpus, onArtifact) }
                composable(Routes.CORPUS) { CorpusScreen(engram, it.arguments?.getString("id").orEmpty(), onArtifact) }
                composable(Routes.DAY) { entry ->
                    // A date that does not parse is today: the route is built
                    // here and never typed, so this is a guard, not a feature.
                    val date = runCatching { LocalDate.parse(entry.arguments?.getString("date")) }
                        .getOrDefault(LocalDate.now(ZoneId.systemDefault()))
                    DayScreen(engram, date, onDay = { go(Routes.day(it)) }, onCorpus, onArtifact)
                }
            }
        }
    }
}

@Composable
fun RefusedBanner(onRescan: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.error.copy(alpha = 0.1f), modifier = Modifier.fillMaxWidth()) {
        Row(
            Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Unpaired on the server · queue holds", style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = onRescan) { Text("Scan a new code") }
        }
    }
}

@Composable
fun PinMismatchScreen(e: PinMismatch, onUnpair: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center) {
        Text("Certificate changed", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.error)
        Spacer(Modifier.height(12.dp))
        Text("Pinned", style = MaterialTheme.typography.labelSmall)
        Text(e.expected, style = MaterialTheme.typography.labelMedium)
        Spacer(Modifier.height(8.dp))
        Text("Served", style = MaterialTheme.typography.labelSmall)
        Text(e.served, style = MaterialTheme.typography.labelMedium)
        Spacer(Modifier.height(24.dp))
        Button(onClick = onUnpair, colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)) {
            Text("Unpair")
        }
    }
}
