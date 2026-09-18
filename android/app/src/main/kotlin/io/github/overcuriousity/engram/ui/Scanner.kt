package io.github.overcuriousity.engram.ui

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.NotFoundException
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.Executors

/** CameraX preview with ZXing on every frame. Calls back once per distinct text. */
@Composable
fun Scanner(modifier: Modifier = Modifier, onText: (String) -> Unit) {
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current
    var granted by remember {
        mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it }
    LaunchedEffect(Unit) { if (!granted) ask.launch(Manifest.permission.CAMERA) }
    if (!granted) {
        Text("Camera · needed to scan", style = MaterialTheme.typography.bodySmall, color = muted())
        return
    }

    val reader = remember {
        MultiFormatReader().apply { setHints(mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE))) }
    }
    var last by remember { mutableStateOf<String?>(null) }
    val executor = remember { Executors.newSingleThreadExecutor() }
    DisposableEffect(Unit) { onDispose { executor.shutdown() } }

    // Framed, so it reads as something to point at. A preview with no edge of
    // its own is a smear of whatever the camera happens to see, and the first
    // person to hold this asked where the scanner was while looking at it.
    AndroidView(
        modifier = modifier
            .clip(MaterialTheme.shapes.medium)
            .border(BorderStroke(1.dp, MaterialTheme.colorScheme.outline), MaterialTheme.shapes.medium),
        factory = { ctx ->
            val view = PreviewView(ctx)
            // COMPATIBLE, which draws through a TextureView. The default,
            // PERFORMANCE, gives the preview a SurfaceView in a window layer
            // of its own: it is not clipped to the slot Compose measured for
            // it and it is drawn over everything else on the screen, which
            // erased the wordmark, the heading and the line telling you where
            // the code comes from.
            view.implementationMode = PreviewView.ImplementationMode.COMPATIBLE
            val future = ProcessCameraProvider.getInstance(ctx)
            future.addListener({
                val provider = future.get()
                val preview = Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
                val analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
                analysis.setAnalyzer(executor) { img ->
                    decode(img, reader)?.let { t -> if (t != last) { last = t; onText(t) } }
                    img.close()
                }
                provider.unbindAll()
                provider.bindToLifecycle(lifecycle, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
            }, ContextCompat.getMainExecutor(ctx))
            view
        },
    )
}

private fun decode(img: ImageProxy, reader: MultiFormatReader): String? {
    val plane = img.planes[0]
    val buf = plane.buffer
    val bytes = ByteArray(buf.remaining())
    buf.get(bytes)
    val src = PlanarYUVLuminanceSource(bytes, plane.rowStride, img.height, 0, 0, img.width, img.height, false)
    return try {
        reader.decodeWithState(BinaryBitmap(HybridBinarizer(src))).text
    } catch (e: NotFoundException) {
        null
    } finally {
        reader.reset()
    }
}
