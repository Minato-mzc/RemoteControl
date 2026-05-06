package com.remotecontrol.app.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.collectAsState
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.remotecontrol.app.model.ConnectionState
import com.remotecontrol.app.model.QrPayload
import com.remotecontrol.app.net.AudioFrame
import com.remotecontrol.app.net.ConnectionClient
import com.remotecontrol.app.net.MouseBtn
import com.remotecontrol.app.net.VideoFrame
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

class AppViewModel : ViewModel() {

    private val client = ConnectionClient()

    val connectionState: StateFlow<ConnectionState> = client.state
    val videoFrames: SharedFlow<VideoFrame> = client.frames
    val audioFrames: SharedFlow<AudioFrame> = client.audioFrames

    private val _lastInvalidQr = MutableStateFlow(false)
    val lastInvalidQr: StateFlow<Boolean> = _lastInvalidQr.asStateFlow()

    private val _framesReceived = MutableStateFlow(0L)
    /** Total binary video frames the WebSocket has received. UI debug overlay. */
    val framesReceived: StateFlow<Long> = _framesReceived.asStateFlow()

    init {
        // Auto-request screen stream on first successful handshake.
        viewModelScope.launch {
            var lastSession: String? = null
            client.state.collect { state ->
                if (state is ConnectionState.Connected && state.session != lastSession) {
                    lastSession = state.session
                    client.requestStream()
                }
                if (state !is ConnectionState.Connected) {
                    lastSession = null
                }
            }
        }
        // Pump the frame counter into a StateFlow so Compose can observe it.
        viewModelScope.launch {
            while (isActive) {
                _framesReceived.value = client.receivedFrameCount.get()
                delay(500)
            }
        }
    }

    fun onQrScanned(raw: String) {
        val parsed = QrPayload.parse(raw)
        if (parsed == null) {
            _lastInvalidQr.value = true
            return
        }
        _lastInvalidQr.value = false
        client.connect(parsed)
    }

    fun disconnect() {
        client.stopStream()
        client.disconnect()
    }

    fun resetError() = client.disconnect()

    fun requestKeyframe() = client.requestKeyframe()

    fun sendMouseMove(xNorm: Float, yNorm: Float) = client.sendMouseMove(xNorm, yNorm)
    fun sendMouseButton(button: MouseBtn, down: Boolean) = client.sendMouseButton(button, down)
    fun sendMouseScroll(dx: Int, dy: Int) = client.sendMouseScroll(dx, dy)

    fun sendKeyText(text: String) = client.sendKeyText(text)
    fun sendKeyTap(vk: Int) = client.sendKeyTap(vk)

    fun sendClipboardSet(text: String) = client.sendClipboardSet(text)
    fun sendClipboardGet() = client.sendClipboardGet()
    val clipboardFromPc = client.clipboardFromPc

    override fun onCleared() {
        client.stopStream()
        client.disconnect()
        super.onCleared()
    }
}

@Composable
fun <T> StateFlow<T>.collectAsStateSafely(): State<T> = collectAsState()
