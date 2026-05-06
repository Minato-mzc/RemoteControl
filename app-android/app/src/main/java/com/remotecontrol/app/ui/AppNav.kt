package com.remotecontrol.app.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import com.remotecontrol.app.model.ConnectionState
import com.remotecontrol.app.net.AudioFrame
import com.remotecontrol.app.net.VideoFrame
import kotlinx.coroutines.flow.SharedFlow

@Composable
fun AppNav(
    state: ConnectionState,
    frames: SharedFlow<VideoFrame>,
    audioFrames: SharedFlow<AudioFrame>,
    clipboardFromPc: SharedFlow<String>,
    framesReceived: Long,
    input: InputCallbacks,
    onScanResult: (String) -> Unit,
    onDisconnect: () -> Unit,
    onResetError: () -> Unit,
) {
    var showScanner by remember { mutableStateOf(false) }

    when {
        showScanner -> ScannerScreen(
            onBack = { showScanner = false },
            onDecoded = { raw ->
                showScanner = false
                onScanResult(raw)
            },
        )
        else -> MainScreen(
            state = state,
            frames = frames,
            audioFrames = audioFrames,
            clipboardFromPc = clipboardFromPc,
            framesReceived = framesReceived,
            input = input,
            onScanClick = { showScanner = true },
            onDisconnect = onDisconnect,
            onResetError = onResetError,
        )
    }
}
