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
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import io.github.overcuriousity.engram.R
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.Mode
import io.github.overcuriousity.engram.core.contained.Core
import io.github.overcuriousity.engram.core.contained.ModelManifest
import kotlinx.coroutines.launch
import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.contained.CoreState
import kotlinx.coroutines.flow.StateFlow
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
    /** What this memory is like, and what the base did on its own. Reached from Settings, like judging. */
    object Insights : Screen("insights", "Insights")
}

/**
 * The places the bar holds. Home is the box, and the box is where capture
 * happens too — there is no Capture place, because there is no second box to
 * go to. The queue and the settings are looked at, not lived in, and sit above.
 */
internal val BAR = listOf(Screen.Search, Screen.Today, Screen.Library)

/**
 * Where a row leads. Ids, dates and the search an open came from travel in
 * the route; so does what fills the box when a door opens it — an answer to
 * edit first — and nothing else does.
 */
private object Routes {
    const val ARTIFACT = "artifact/{id}?event={event}"
    const val CORPUS = "corpus/{id}?from={from}&to={to}"
    const val DAY = "day/{date}"
    const val ASK = "ask?q={q}"
    const val SEARCH = "search?prefill={prefill}&from_ask={from_ask}&question={question}"
    fun artifact(id: String, event: String? = null) = "artifact/${Uri.encode(id)}" + (event?.let { "?event=${Uri.encode(it)}" } ?: "")
    fun corpus(id: String, from: Long? = null, to: Long? = null) =
        "corpus/${Uri.encode(id)}" + (if (from != null) "?from=$from&to=${to ?: from}" else "")
    fun day(date: LocalDate) = "day/$date"
    fun ask(q: String) = "ask?q=${Uri.encode(q)}"
    fun search(prefill: String, fromAsk: String, question: String) =
        "search?prefill=${Uri.encode(prefill)}&from_ask=${Uri.encode(fromAsk)}&question=${Uri.encode(question)}"
    val optional = listOf("prefill", "from_ask", "question", "event", "from", "to").map { navArgument(it) { type = NavType.StringType; defaultValue = "" } }
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
    val connection by engram.connection.collectAsStateWithLifecycle()
    val refused by engram.refused.collectAsStateWithLifecycle()
    val pinned by engram.pinMismatch.collectAsStateWithLifecycle()
    val nav = rememberNavController()

    if (pinned != null) {
        PinMismatchScreen(pinned!!, onUnpair = onUnpair)
        return
    }
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    // A new install chooses where its engram lives. One that holds a
    // connection never sees this, and a pairing code goes straight to pairing.
    var pairing by rememberSaveable { mutableStateOf(false) }
    if (connection == null && engram.modes.chosen == null && pairText == null && !pairing) {
        ModeChooser(
            Core.available, sizeWords(ModelManifest.required.sumOf { it.bytes }),
            onPhone = { scope.launch { Engram.switch(ctx, Mode.contained) } },
            onServer = { pairing = true },
        )
        return
    }
    if (engram.loopback) {
        var missing by remember { mutableStateOf(engram.requiredMissing()) }
        if (missing.isNotEmpty()) {
            DownloadScreen(
                engram, missing, onChanged = { missing = engram.requiredMissing() },
                onServer = { scope.launch { Engram.switch(ctx, Mode.server) } },
            )
            return
        }
    }
    if (connection == null) {
        val core = engram.core
        if (core == null) PairScreen(engram, initialText = pairText) else CoreStarting(engram, core)
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
                // The route as registered carries its optional arguments; the
                // bar's word is what stands before them.
                val current = nav.currentBackStackEntryAsState().value?.destination?.route?.substringBefore('?')
                bar.forEach { s ->
                    NavigationBarItem(
                        selected = current == s.route,
                        onClick = { nav.navigate(s.route) { launchSingleTop = true; popUpTo(Routes.SEARCH) } },
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
            // Home is registered with its optional arguments, and the graph
            // starts on the route as registered.
            NavHost(nav, startDestination = if (startAt == Screen.Search) Routes.SEARCH else startAt.route) {
                composable(Routes.SEARCH, arguments = Routes.optional) { entry ->
                    val arg = { k: String -> entry.arguments?.getString(k).orEmpty() }
                    SearchScreen(
                        engram,
                        onArtifact = onArtifact,
                        onHit = { id, event -> go(Routes.artifact(id, event)) },
                        onAsk = { go(Routes.ask(it)) },
                        onCorpus = onCorpus,
                        focusBox = focusBox,
                        prefill = arg("prefill"),
                        fromAsk = arg("from_ask").ifEmpty { null },
                        question = arg("question"),
                    )
                }
                composable(Screen.Today.route) {
                    DayScreen(engram, LocalDate.now(ZoneId.systemDefault()), onDay = { go(Routes.day(it)) }, onCorpus, onArtifact)
                }
                composable(Screen.Library.route) { LibraryScreen(engram, onCorpus) }
                composable(Screen.Queue.route) { QueueScreen(engram) }
                composable(Screen.Settings.route) {
                    SettingsScreen(engram, onJudging = { go(it.route) }, onQueue = { go(Screen.Queue.route) }, onUnpair = onUnpair)
                }
                composable(Screen.Insights.route) {
                    InsightsScreen(engram, onJudging = { go(it.route) }, onArtifact, onCorpus, onLibrary = { go(Screen.Library.route) })
                }
                composable(Screen.Pairs.route) { PairReviewScreen(engram, onArtifact) }
                composable(Screen.Gaps.route) { GapsScreen(engram) }
                composable(Screen.Journal.route) { JournalScreen(engram, onArtifact, onCorpus) }
                composable(Screen.Pair.route) { PairScreen(engram, initialText = pairText, knownOrigin = connection?.origin) }
                composable(Routes.ASK) {
                    AskScreen(
                        engram, it.arguments?.getString("q").orEmpty(), onArtifact, onCorpus,
                        onEditFirst = { answer, event, q -> go(Routes.search(answer, event, q)) },
                        onSettings = { go(Screen.Settings.route) },
                    )
                }
                composable(Routes.ARTIFACT, arguments = Routes.optional) { entry ->
                    ArtifactScreen(
                        engram, entry.arguments?.getString("id").orEmpty(),
                        onCorpus = { id, from, to -> go(Routes.corpus(id, from, to)) },
                        onArtifact = onArtifact,
                        event = entry.arguments?.getString("event")?.ifEmpty { null },
                    )
                }
                composable(Routes.CORPUS, arguments = Routes.optional) { entry ->
                    CorpusScreen(
                        engram, entry.arguments?.getString("id").orEmpty(), onArtifact,
                        highlight = entry.arguments?.getString("from")?.toLongOrNull()?.let { f -> f to (entry.arguments?.getString("to")?.toLongOrNull() ?: f) },
                        onGone = { nav.popBackStack() },
                    )
                }
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

/** Contained mode before the core answers: a word while it starts, and the reason where it cannot. */
@Composable
fun CoreStarting(engram: Engram, core: StateFlow<CoreState>) {
    val state by core.collectAsStateWithLifecycle()
    LaunchedEffect(Unit) { engram.ready() }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center) {
        when (val s = state) {
            is CoreState.Unavailable -> {
                Text("On this phone · unavailable", style = MaterialTheme.typography.titleLarge)
                Spacer(Modifier.height(12.dp))
                Text(s.why, style = MaterialTheme.typography.labelMedium)
            }
            else -> Text("Starting", style = MaterialTheme.typography.titleLarge)
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
