package com.remotecontrol.app.ui

import android.graphics.SurfaceTexture
import android.os.SystemClock
import android.view.Surface
import android.view.TextureView
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.remotecontrol.app.model.ActiveStream
import com.remotecontrol.app.model.AudioStreamInfo
import com.remotecontrol.app.net.AudioFrame
import com.remotecontrol.app.net.Macro
import com.remotecontrol.app.net.MouseBtn
import com.remotecontrol.app.net.VideoFrame
import com.remotecontrol.app.video.H264Player
import com.remotecontrol.app.video.OpusPlayer
import com.remotecontrol.app.video.videoMimeFor
import kotlinx.coroutines.flow.SharedFlow

/** Lambdas the StreamSurface and KeyboardOverlay invoke for PC input. */
data class InputCallbacks(
    val onMove: (xNorm: Float, yNorm: Float) -> Unit,
    val onButton: (button: MouseBtn, down: Boolean) -> Unit,
    val onScroll: (dx: Int, dy: Int) -> Unit,
    val onKeyText: (String) -> Unit = {},
    val onKeyTap: (vk: Int) -> Unit = {},
    val onClipboardPush: (String) -> Unit = {},
    val onClipboardPull: () -> Unit = {},
    val onMacro: (Macro) -> Unit = {},
)

@Composable
fun StreamSurface(
    stream: ActiveStream,
    frames: SharedFlow<VideoFrame>,
    input: InputCallbacks,
    modifier: Modifier = Modifier,
) {
    val player = remember(stream.streamId) { H264Player() }
    var surfaceReady by remember(stream.streamId) { mutableStateOf(false) }
    var initError by remember(stream.streamId) { mutableStateOf<String?>(null) }
    val density = LocalDensity.current
    // ~32dp of finger movement per scroll wheel notch — feels close to a desktop trackpad.
    val scrollPxPerNotch = remember(density) { with(density) { 32.dp.toPx() } }

    DisposableEffect(stream.streamId) { onDispose { player.stop() } }

    val aspect = if (stream.height > 0) stream.width.toFloat() / stream.height else 16f / 9f

    Box(modifier = modifier.aspectRatio(aspect, matchHeightConstraintsFirst = true)) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                TextureView(ctx).apply {
                    surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                        override fun onSurfaceTextureAvailable(
                            st: SurfaceTexture,
                            width: Int,
                            height: Int,
                        ) {
                            val mime = videoMimeFor(stream.codec)
                            val r = player.start(Surface(st), mime, stream.width, stream.height)
                            if (r.isFailure) {
                                initError = r.exceptionOrNull()?.message ?: "codec init failed"
                            } else {
                                surfaceReady = true
                            }
                        }

                        override fun onSurfaceTextureSizeChanged(
                            st: SurfaceTexture,
                            width: Int,
                            height: Int,
                        ) { /* codec keeps its own size */ }

                        override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
                            surfaceReady = false
                            player.stop()
                            return true
                        }

                        override fun onSurfaceTextureUpdated(st: SurfaceTexture) { /* no-op */ }
                    }
                }
            },
        )

        // Transparent touch capture overlay sitting above the TextureView.
        // Compose's PointerInput plays poorly with native View interop — the
        // TextureView swallows the input by default. An overlay Box that
        // matches the parent size receives all gestures first.
        Box(
            modifier = Modifier
                .matchParentSize()
                .pointerInput(stream.streamId) { gestureLoop(input, scrollPxPerNotch) },
        )

        if (initError != null) {
            Box(
                modifier = Modifier.fillMaxSize().padding(16.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text("解码器启动失败：${initError}", color = Color(0xFFFF6B6B))
            }
        }
    }

    LaunchedEffect(stream.streamId, surfaceReady) {
        if (!surfaceReady) return@LaunchedEffect
        frames.collect { frame -> player.feed(frame) }
    }
}

/**
 * Headless playback effect for the Opus audio sub-stream. Owns an
 * `OpusPlayer` for the lifetime of the given `AudioStreamInfo` and pumps
 * frames into it. AudioTrack drives playback timing; video sync is implicit
 * via the shared PTS clock (see H264Player.feed using renderTimeNs).
 */
@Composable
fun AudioPlaybackEffect(
    audio: AudioStreamInfo,
    frames: SharedFlow<AudioFrame>,
) {
    val player = remember(audio) { OpusPlayer() }
    DisposableEffect(audio) {
        player.start(audio)
        onDispose { player.stop() }
    }
    LaunchedEffect(audio, frames) {
        frames.collect { player.feed(it) }
    }
}

/**
 * Gesture state machine running for the lifetime of the pointerInput.
 *
 * Per gesture (first finger down → all fingers up):
 *  - 1 finger, short tap → move-to + left click
 *  - 1 finger, double tap → second click without move (preserves Win double-click)
 *  - 1 finger, long press (≥500ms, no movement) → move-to + right click
 *  - 1 finger, movement → drag = left button down + continuous move + up
 *  - 2 fingers, vertical drag → scroll wheel notches (natural-scroll mapping:
 *    swipe up = wheel down, swipe down = wheel up)
 *
 * Two-finger tap was tried as a right-click trigger but proved unreliable on
 * narrow targets (e.g. taskbar) — replaced by single-finger long-press.
 * Two-finger horizontal scroll is intentionally not wired in M3.
 */
private suspend fun PointerInputScope.gestureLoop(
    cb: InputCallbacks,
    scrollPxPerNotch: Float,
) {
    val touchSlopSq = viewConfiguration.touchSlop * viewConfiguration.touchSlop
    val tapTimeoutMs = 300L
    val longPressMs = 500L
    // Two single-taps within this window + within this radius become a Windows
    // double-click. We deliberately suppress the second tap's mouse_move so
    // the two SendInput clicks land at the same Win32 cursor position — Windows
    // requires "no movement > SM_CXDOUBLECLK (≈4px)" between the two clicks
    // to recognize the double-click and launch the icon.
    val doubleTapTimeoutMs = 350L
    // PointerInputScope inherits Density, so .toPx() is in scope here.
    val doubleTapSlopPx = 48.dp.toPx()
    val doubleTapSlopSq = doubleTapSlopPx * doubleTapSlopPx

    var lastTapTime = 0L
    var lastTapX = 0f
    var lastTapY = 0f

    awaitEachGesture {
        val firstDown = awaitFirstDown(requireUnconsumed = false)
        val downPos = firstDown.position
        val downTime = SystemClock.uptimeMillis()
        var singleFingerMoved = false
        var leftDownSent = false
        var sawTwoFingers = false
        var twoFingerScrolled = false
        var lastMidY: Float? = null
        var scrollAccum = 0f

        while (true) {
            val event = awaitPointerEvent()
            val active = event.changes.filter { it.pressed }
            val n = active.size
            if (n == 0) break

            if (n == 1) {
                val p = active[0].position
                val dxp = p.x - downPos.x
                val dyp = p.y - downPos.y
                if (!singleFingerMoved && (dxp * dxp + dyp * dyp) > touchSlopSq) {
                    singleFingerMoved = true
                }
                // Once a second finger has touched down, we're committed to a
                // two-finger gesture for the rest of this stroke — even if the
                // user briefly drops back to one finger. Don't start a left
                // drag in that case.
                if (singleFingerMoved && !sawTwoFingers) {
                    val xn = (p.x / size.width.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                    val yn = (p.y / size.height.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                    cb.onMove(xn, yn)
                    if (!leftDownSent) {
                        cb.onButton(MouseBtn.Left, true)
                        leftDownSent = true
                    }
                }
                // resetting midpoint avoids a stale baseline if 2nd finger lifts and lands again
                lastMidY = null
                scrollAccum = 0f
            } else {
                // 2+ fingers — scroll on vertical midpoint movement
                sawTwoFingers = true
                val a = active[0].position
                val b = active[1].position
                val midY = (a.y + b.y) / 2f
                if (lastMidY == null) {
                    lastMidY = midY
                } else {
                    // Natural-scroll mapping: swiping fingers UP scrolls page DOWN,
                    // mirroring trackpad behavior on macOS / modern Windows.
                    val deltaPx = midY - lastMidY!! // up swipe → negative
                    scrollAccum += deltaPx
                    val notches = (scrollAccum / scrollPxPerNotch).toInt()
                    if (notches != 0) {
                        cb.onScroll(0, notches)
                        scrollAccum -= notches * scrollPxPerNotch
                        twoFingerScrolled = true
                    }
                    lastMidY = midY
                }
            }
        }

        // gesture ended
        val dur = SystemClock.uptimeMillis() - downTime
        when {
            leftDownSent -> cb.onButton(MouseBtn.Left, false)
            // Two-finger gestures only ever produce scroll; if the user lifted
            // without crossing the scroll threshold we just discard the gesture.
            sawTwoFingers -> Unit
            !singleFingerMoved && dur >= longPressMs -> {
                // Single-finger long press → right click. Especially useful on
                // narrow targets like the Windows taskbar where two fingers
                // both have to land inside ~40px and that's hard to do.
                val xn = (downPos.x / size.width.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                val yn = (downPos.y / size.height.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                cb.onMove(xn, yn)
                cb.onButton(MouseBtn.Right, true)
                cb.onButton(MouseBtn.Right, false)
            }
            !singleFingerMoved && dur < tapTimeoutMs -> {
                val now = SystemClock.uptimeMillis()
                val ddx = downPos.x - lastTapX
                val ddy = downPos.y - lastTapY
                val isDoubleTap = (now - lastTapTime) < doubleTapTimeoutMs &&
                        (ddx * ddx + ddy * ddy) < doubleTapSlopSq

                if (isDoubleTap) {
                    // Skip mouse_move so the second click stays at the first
                    // click's exact coordinate — preserves Windows double-click.
                    cb.onButton(MouseBtn.Left, true)
                    cb.onButton(MouseBtn.Left, false)
                    lastTapTime = 0L // consume; next tap starts fresh
                } else {
                    val xn = (downPos.x / size.width.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                    val yn = (downPos.y / size.height.coerceAtLeast(1).toFloat()).coerceIn(0f, 1f)
                    cb.onMove(xn, yn)
                    cb.onButton(MouseBtn.Left, true)
                    cb.onButton(MouseBtn.Left, false)
                    lastTapTime = now
                    lastTapX = downPos.x
                    lastTapY = downPos.y
                }
            }
        }
    }
}
