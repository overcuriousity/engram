package io.github.overcuriousity.engram.ui

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

sealed class Screen(val route: String, val label: String) {
    object Pair : Screen("pair", "Pair")
    object Compose : Screen("compose", "Capture")
    object Queue : Screen("queue", "Queue")
    object Settings : Screen("settings", "Settings")
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
fun EngramApp(engram: Engram, start: Screen? = null, pairText: String? = null, onUnpair: () -> Unit) {
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

    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = { Wordmark() },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.background),
            )
        },
        bottomBar = {
            NavigationBar(containerColor = MaterialTheme.colorScheme.surface) {
                val current = nav.currentBackStackEntryAsState().value?.destination?.route
                listOf(Screen.Compose, Screen.Queue, Screen.Settings).forEach { s ->
                    NavigationBarItem(
                        selected = current == s.route,
                        onClick = { nav.navigate(s.route) { launchSingleTop = true; popUpTo(Screen.Compose.route) } },
                        icon = {},
                        label = { Text(s.label) },
                    )
                }
            }
        },
    ) { pad ->
        Column(Modifier.padding(pad)) {
            if (refused) RefusedBanner(onRescan = { nav.navigate(Screen.Pair.route) })
            NavHost(nav, startDestination = (start ?: Screen.Compose).route) {
                composable(Screen.Compose.route) { ComposeScreen(engram) }
                composable(Screen.Queue.route) { QueueScreen(engram) }
                composable(Screen.Settings.route) { SettingsScreen(engram, onUnpair = onUnpair) }
                composable(Screen.Pair.route) { PairScreen(engram, initialText = null, knownOrigin = connection?.origin) }
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
