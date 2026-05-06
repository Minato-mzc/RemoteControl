package com.remotecontrol.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import com.remotecontrol.app.ui.AppNav
import com.remotecontrol.app.ui.AppViewModel
import com.remotecontrol.app.ui.InputCallbacks
import com.remotecontrol.app.ui.collectAsStateSafely

class MainActivity : ComponentActivity() {

    private val vm: AppViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier, color = MaterialTheme.colorScheme.background) {
                    val state by vm.connectionState.collectAsStateSafely()
                    val framesReceived by vm.framesReceived.collectAsStateSafely()
                    val input = remember(vm) {
                        InputCallbacks(
                            onMove = vm::sendMouseMove,
                            onButton = vm::sendMouseButton,
                            onScroll = vm::sendMouseScroll,
                            onKeyText = vm::sendKeyText,
                            onKeyTap = vm::sendKeyTap,
                            onClipboardPush = vm::sendClipboardSet,
                            onClipboardPull = vm::sendClipboardGet,
                        )
                    }
                    AppNav(
                        state = state,
                        frames = vm.videoFrames,
                        audioFrames = vm.audioFrames,
                        clipboardFromPc = vm.clipboardFromPc,
                        framesReceived = framesReceived,
                        input = input,
                        onScanResult = vm::onQrScanned,
                        onDisconnect = vm::disconnect,
                        onResetError = vm::resetError,
                    )
                }
            }
        }
    }
}
