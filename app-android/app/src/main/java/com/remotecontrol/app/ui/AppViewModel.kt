package com.remotecontrol.app.ui

import android.app.Application
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.collectAsState
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.remotecontrol.app.model.ConnectionState
import com.remotecontrol.app.model.QrPayload
import com.remotecontrol.app.net.AudioFrame
import com.remotecontrol.app.net.ConnectionClient
import com.remotecontrol.app.net.Macro
import com.remotecontrol.app.net.MacroStep
import com.remotecontrol.app.net.MouseBtn
import com.remotecontrol.app.net.TrustedServer
import com.remotecontrol.app.net.TrustedServerStore
import com.remotecontrol.app.net.VideoFrame
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

class AppViewModel(app: Application) : AndroidViewModel(app) {

    private val client = ConnectionClient()
    private val trustedStore = TrustedServerStore(app)

    val connectionState: StateFlow<ConnectionState> = client.state
    val videoFrames: SharedFlow<VideoFrame> = client.frames
    val audioFrames: SharedFlow<AudioFrame> = client.audioFrames

    private val _lastInvalidQr = MutableStateFlow(false)
    val lastInvalidQr: StateFlow<Boolean> = _lastInvalidQr.asStateFlow()

    private val _framesReceived = MutableStateFlow(0L)
    /** Total binary video frames the WebSocket has received. UI debug overlay. */
    val framesReceived: StateFlow<Long> = _framesReceived.asStateFlow()

    /**
     * Servers we've previously paired with. Idle screen reads this to show
     * "重新连接 ADMIN" buttons. Refreshed on init and after every save.
     */
    private val _trustedServers = MutableStateFlow<List<TrustedServer>>(emptyList())
    val trustedServers: StateFlow<List<TrustedServer>> = _trustedServers.asStateFlow()

    init {
        _trustedServers.value = trustedStore.list()

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
        // Persist freshly-minted trust tokens so the next app launch can
        // skip QR. The ConnectionClient drops a TrustedServer here exactly
        // once per successful QR-pairing handshake.
        viewModelScope.launch {
            client.newlyTrustedServer.collect { trusted ->
                if (trusted != null) {
                    trustedStore.upsert(trusted)
                    _trustedServers.value = trustedStore.list()
                }
            }
        }
        // Drop stale entries when the server says it doesn't know us. The
        // user will be told ("信任凭证已失效") and the Idle screen no longer
        // shows that PC's reconnect button.
        viewModelScope.launch {
            client.forgetDeviceId.collect { deviceId ->
                trustedStore.forget(deviceId)
                _trustedServers.value = trustedStore.list()
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

    /** Re-open a previously-trusted server without scanning. */
    fun reconnectTrusted(server: TrustedServer) {
        _lastInvalidQr.value = false
        client.connectTrusted(server)
    }

    /** Manual "this PC is wrong, drop it" — for the trusted-list UI. */
    fun forgetTrustedServer(deviceId: String) {
        trustedStore.forget(deviceId)
        _trustedServers.value = trustedStore.list()
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

    /** Run a macro by sequentially shipping its key_event steps. */
    fun runMacro(macro: Macro) {
        viewModelScope.launch {
            for (step in macro.steps) {
                when (step) {
                    is MacroStep.KeyDown -> client.sendKeyEvent(step.vk, true)
                    is MacroStep.KeyUp -> client.sendKeyEvent(step.vk, false)
                    is MacroStep.KeyTap -> client.sendKeyTap(step.vk)
                    is MacroStep.Delay -> delay(step.ms)
                }
            }
        }
    }

    override fun onCleared() {
        client.stopStream()
        client.disconnect()
        super.onCleared()
    }
}

@Composable
fun <T> StateFlow<T>.collectAsStateSafely(): State<T> = collectAsState()
