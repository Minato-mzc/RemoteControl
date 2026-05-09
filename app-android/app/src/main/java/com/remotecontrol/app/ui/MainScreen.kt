package com.remotecontrol.app.ui

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import androidx.compose.foundation.background
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.QrCodeScanner
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import android.widget.Toast
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.remotecontrol.app.model.ConnectionState
import com.remotecontrol.app.net.AudioFrame
import com.remotecontrol.app.net.TrustedServer
import com.remotecontrol.app.net.VideoFrame
import kotlinx.coroutines.flow.SharedFlow

@Composable
fun MainScreen(
    state: ConnectionState,
    frames: SharedFlow<VideoFrame>,
    audioFrames: SharedFlow<AudioFrame>,
    clipboardFromPc: SharedFlow<String>,
    framesReceived: Long,
    input: InputCallbacks,
    trustedServers: List<TrustedServer>,
    onScanClick: () -> Unit,
    onReconnect: (TrustedServer) -> Unit,
    onForgetTrusted: (String) -> Unit,
    onDisconnect: () -> Unit,
    onResetError: () -> Unit,
) {
    // Apply PC clipboard text the moment the server replies, regardless of
    // which screen the user happens to be on.
    val context = LocalContext.current
    LaunchedEffect(clipboardFromPc) {
        clipboardFromPc.collect { text ->
            writePhoneClipboard(context, text)
            val msg = if (text.isEmpty()) "PC 剪贴板为空" else "已写入手机剪贴板"
            Toast.makeText(context, msg, Toast.LENGTH_SHORT).show()
        }
    }
    when (state) {
        is ConnectionState.Connected -> ConnectedScreen(state, frames, audioFrames, framesReceived, input, onDisconnect)
        else -> CenteredColumn {
            Header()
            Spacer(Modifier.height(40.dp))
            when (state) {
                is ConnectionState.Idle -> IdleBlock(
                    trustedServers = trustedServers,
                    onScanClick = onScanClick,
                    onReconnect = onReconnect,
                    onForget = onForgetTrusted,
                )
                is ConnectionState.Connecting -> ConnectingBlock()
                is ConnectionState.Failed -> FailedBlock(
                    reason = state.reason,
                    trustedServers = trustedServers,
                    onResetError = onResetError,
                    onScanClick = onScanClick,
                    onReconnect = onReconnect,
                )
                is ConnectionState.Connected -> Unit // unreachable
            }
        }
    }
}

@Composable
private fun ConnectedScreen(
    state: ConnectionState.Connected,
    frames: SharedFlow<VideoFrame>,
    audioFrames: SharedFlow<AudioFrame>,
    framesReceived: Long,
    input: InputCallbacks,
    onDisconnect: () -> Unit,
) {
    // While streaming, let the device's orientation sensor drive UI rotation
    // so the user can flip the phone landscape for a wider view of the
    // 16:9 PC desktop. Restore portrait when this screen exits.
    val activity = LocalContext.current.findActivity()
    DisposableEffect(activity) {
        activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_FULL_SENSOR
        onDispose {
            activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
        }
    }

    // Hide status + nav bars so the phone's system UI doesn't cover the top
    // of the PC desktop (which the user couldn't tap before) and isn't
    // stealing touches from Windows' bottom taskbar. Bars come back on
    // swipe-from-edge, and are restored when this screen leaves.
    val view = LocalView.current
    DisposableEffect(activity, view) {
        val window = activity?.window
        if (window != null) {
            val controller = WindowInsetsControllerCompat(window, view)
            controller.hide(WindowInsetsCompat.Type.systemBars())
            controller.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        onDispose {
            if (window != null) {
                WindowInsetsControllerCompat(window, view)
                    .show(WindowInsetsCompat.Type.systemBars())
            }
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
        contentAlignment = Alignment.Center,
    ) {
        val stream = state.stream
        if (stream != null) {
            // Video centered, scaled to fit screen while preserving aspect.
            // matchHeightConstraintsFirst = true so a wide landscape window
            // fills by height (sides letterbox) instead of by width (which
            // would clip vertically).
            StreamSurface(
                stream = stream,
                frames = frames,
                input = input,
                modifier = Modifier,
            )

            // Top-left overlay: server + stream metadata. Translucent on dark.
            Column(
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .padding(12.dp),
            ) {
                Text(
                    "${state.serverName} · ${stream.width}×${stream.height}@${stream.fps}fps",
                    color = Color.White,
                    fontSize = 12.sp,
                )
                Text(
                    "${stream.codec.uppercase()} · ${stream.bitrateKbps} kbps · session ${state.session.take(8)}…",
                    color = Color(0xFFB0B0B0),
                    fontSize = 10.sp,
                )
                Text(
                    "rx frames: $framesReceived",
                    color = Color(0xFF80D0A0),
                    fontSize = 10.sp,
                )
            }

            // Bottom-left disconnect button.
            OutlinedButton(
                onClick = onDisconnect,
                modifier = Modifier
                    .align(Alignment.BottomStart)
                    .padding(12.dp),
            ) { Text("断开连接") }

            // Floating keyboard overlay (its own collapsed/expanded state).
            KeyboardOverlay(
                onKeyText = input.onKeyText,
                onKeyTap = input.onKeyTap,
                onClipboardPush = input.onClipboardPush,
                onClipboardPull = input.onClipboardPull,
                onMacro = input.onMacro,
                modifier = Modifier.fillMaxSize(),
            )

            // Audio playback (headless effect — no UI). When the server didn't
            // start an audio sub-stream, stream.audio is null and we skip.
            stream.audio?.let { AudioPlaybackEffect(it, audioFrames) }
        } else {
            Column(
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                CircularProgressIndicator(color = Color.White)
                Spacer(Modifier.height(16.dp))
                Text("正在请求屏幕串流…", color = Color.White)
                Spacer(Modifier.height(24.dp))
                OutlinedButton(onClick = onDisconnect) { Text("断开连接") }
            }
        }
    }
}

/** Walks ContextWrapper chain to the underlying Activity. */
private fun Context.findActivity(): Activity? {
    var c: Context = this
    while (c is ContextWrapper) {
        if (c is Activity) return c
        c = c.baseContext
    }
    return null
}

@Composable
private fun CenteredColumn(content: @Composable () -> Unit) {
    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) { content() }
}

@Composable
private fun Header() {
    Text("RemoteControl", fontSize = 28.sp, fontWeight = FontWeight.Bold)
    Spacer(Modifier.height(8.dp))
    Text("手机远程操控电脑", fontSize = 14.sp)
}

@Composable
private fun IdleBlock(
    trustedServers: List<TrustedServer>,
    onScanClick: () -> Unit,
    onReconnect: (TrustedServer) -> Unit,
    onForget: (String) -> Unit,
) {
    Text("未连接", fontSize = 16.sp)
    Spacer(Modifier.height(24.dp))
    if (trustedServers.isNotEmpty()) {
        TrustedServersList(
            servers = trustedServers,
            onReconnect = onReconnect,
            onForget = onForget,
        )
        Spacer(Modifier.height(20.dp))
        Text("或者", fontSize = 13.sp, color = Color(0xFF888888))
        Spacer(Modifier.height(12.dp))
    }
    Button(onClick = onScanClick) {
        Icon(Icons.Default.QrCodeScanner, contentDescription = null)
        Spacer(Modifier.size(8.dp))
        Text(if (trustedServers.isEmpty()) "扫码连接电脑" else "扫描新二维码")
    }
}

/**
 * Vertical list of "Reconnect to PCNAME" rows. Each row has a primary
 * action (left tap = reconnect) and a small × button (right tap = forget,
 * for when a saved server is permanently gone or you re-installed Windows
 * and the server-side trusted_devices.json wiped).
 */
@Composable
private fun TrustedServersList(
    servers: List<TrustedServer>,
    onReconnect: (TrustedServer) -> Unit,
    onForget: (String) -> Unit,
) {
    Column(modifier = Modifier.padding(horizontal = 12.dp)) {
        Text(
            "已配对的电脑",
            fontSize = 13.sp,
            color = Color(0xFF666666),
            modifier = Modifier.padding(start = 4.dp, bottom = 6.dp),
        )
        for (server in servers) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Button(
                    onClick = { onReconnect(server) },
                    modifier = Modifier.fillMaxWidth().weight(1f),
                ) {
                    Icon(Icons.Default.History, contentDescription = null)
                    Spacer(Modifier.size(8.dp))
                    Text(
                        "重新连接 ${server.serverName.ifEmpty { "电脑" }}",
                    )
                }
                Spacer(Modifier.width(6.dp))
                OutlinedButton(
                    onClick = { onForget(server.deviceId) },
                    contentPadding = androidx.compose.foundation.layout.PaddingValues(8.dp),
                    modifier = Modifier.size(48.dp),
                ) {
                    Icon(
                        Icons.Default.Close,
                        contentDescription = "忘记 ${server.serverName}",
                    )
                }
            }
        }
    }
}

@Composable
private fun ConnectingBlock() {
    CircularProgressIndicator()
    Spacer(Modifier.height(16.dp))
    Text("正在连接…")
}

@Composable
private fun FailedBlock(
    reason: String,
    trustedServers: List<TrustedServer>,
    onResetError: () -> Unit,
    onScanClick: () -> Unit,
    onReconnect: (TrustedServer) -> Unit,
) {
    Text("连接失败", fontSize = 18.sp, fontWeight = FontWeight.SemiBold)
    Spacer(Modifier.height(12.dp))
    Text(reason)
    Spacer(Modifier.height(24.dp))
    // If trusted servers exist, offer them as the primary recovery path —
    // the user is much more likely to retry the same PC than to give up.
    // Falling back to the QR scanner always remains available below.
    if (trustedServers.isNotEmpty()) {
        for (server in trustedServers) {
            Button(
                onClick = {
                    onResetError()
                    onReconnect(server)
                },
                modifier = Modifier.padding(vertical = 4.dp),
            ) {
                Icon(Icons.Default.History, contentDescription = null)
                Spacer(Modifier.size(8.dp))
                Text("重试 ${server.serverName.ifEmpty { "电脑" }}")
            }
        }
        Spacer(Modifier.height(12.dp))
    }
    OutlinedButton(onClick = {
        onResetError()
        onScanClick()
    }) { Text("重新扫码") }
}
