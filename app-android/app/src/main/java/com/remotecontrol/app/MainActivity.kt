package com.remotecontrol.app

import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import com.remotecontrol.app.net.FileTransferEvent
import com.remotecontrol.app.ui.AppNav
import com.remotecontrol.app.ui.AppViewModel
import com.remotecontrol.app.ui.InputCallbacks
import com.remotecontrol.app.ui.collectAsStateSafely

/** SAF Uri → (display-name, size). Both come from `OpenableColumns` if
 *  available; falls back to last URI path segment + 0 if the provider
 *  doesn't expose them. */
private fun queryNameAndSize(
    context: android.content.Context,
    uri: Uri,
): Pair<String, Long> {
    var name = uri.lastPathSegment ?: "upload.bin"
    var size = 0L
    context.contentResolver.query(uri, null, null, null, null)?.use { c ->
        val nameIdx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
        val sizeIdx = c.getColumnIndex(OpenableColumns.SIZE)
        if (c.moveToFirst()) {
            if (nameIdx >= 0) c.getString(nameIdx)?.let { name = it }
            if (sizeIdx >= 0) size = c.getLong(sizeIdx)
        }
    }
    return name to size
}

private fun humanBytes(b: Long): String = when {
    b < 1024 -> "$b B"
    b < 1024L * 1024 -> "%.1f KB".format(b / 1024.0)
    b < 1024L * 1024 * 1024 -> "%.1f MB".format(b / 1024.0 / 1024.0)
    else -> "%.1f GB".format(b / 1024.0 / 1024.0 / 1024.0)
}

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
                    val trustedServers by vm.trustedServers.collectAsStateSafely()
                    val uploads by vm.uploads.collectAsStateSafely()
                    val context = LocalContext.current
                    val onUploadFile: (Uri) -> Unit = remember(vm, context) {
                        { uri ->
                            // SAF URIs need contentResolver to extract
                            // display-name and size; both are nullable so
                            // we fall back to sensible defaults.
                            val (name, size) = queryNameAndSize(context, uri)
                            Toast.makeText(
                                context, "正在上传 $name (${humanBytes(size)})",
                                Toast.LENGTH_SHORT,
                            ).show()
                            vm.uploadFile(name, size) {
                                context.contentResolver.openInputStream(uri)
                                    ?: throw java.io.IOException(
                                        "openInputStream returned null for $uri",
                                    )
                            }
                        }
                    }
                    val input = remember(vm, onUploadFile) {
                        InputCallbacks(
                            onMove = vm::sendMouseMove,
                            onButton = vm::sendMouseButton,
                            onScroll = vm::sendMouseScroll,
                            onKeyText = vm::sendKeyText,
                            onKeyTap = vm::sendKeyTap,
                            onClipboardPush = vm::sendClipboardSet,
                            onClipboardPull = vm::sendClipboardGet,
                            onMacro = vm::runMacro,
                            onUploadFile = onUploadFile,
                        )
                    }
                    // Surface upload progress / completion / failure as
                    // toasts. Lives at this scope so the messages keep
                    // showing even if the user collapses the keyboard
                    // panel or backgrounds the app momentarily.
                    LaunchedEffect(vm) {
                        vm.fileEvents.collect { event ->
                            val msg = when (event) {
                                is FileTransferEvent.Accepted ->
                                    "PC 已接收，开始上传…"
                                is FileTransferEvent.Complete ->
                                    "上传完成：${event.destPath}"
                                is FileTransferEvent.Failed ->
                                    "上传失败：${event.reason}"
                                is FileTransferEvent.Progress -> null
                            }
                            if (msg != null) {
                                Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
                            }
                        }
                    }
                    AppNav(
                        state = state,
                        frames = vm.videoFrames,
                        audioFrames = vm.audioFrames,
                        clipboardFromPc = vm.clipboardFromPc,
                        framesReceived = framesReceived,
                        uploads = uploads,
                        input = input,
                        trustedServers = trustedServers,
                        onScanResult = vm::onQrScanned,
                        onReconnect = vm::reconnectTrusted,
                        onForgetTrusted = vm::forgetTrustedServer,
                        onDisconnect = vm::disconnect,
                        onResetError = vm::resetError,
                    )
                }
            }
        }
    }
}
