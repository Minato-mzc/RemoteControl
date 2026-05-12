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
import com.remotecontrol.app.net.FileTransferEvent
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

    // App-private external-storage Downloads subdir. Visible in the
    // system file manager but doesn't need a runtime storage permission
    // (scoped storage takes care of it for our own files). Created the
    // first time the ConnectionClient lands a file there.
    //
    // Public so the "received files" dialog in MainScreen can enumerate
    // its contents and hand them off to ACTION_VIEW via FileProvider.
    val downloadsDir: java.io.File =
        java.io.File(app.getExternalFilesDir(null), "Downloads")

    private val client = ConnectionClient(downloadsDir = downloadsDir)
    private val trustedStore = TrustedServerStore(app)

    val connectionState: StateFlow<ConnectionState> = client.state
    val videoFrames: SharedFlow<VideoFrame> = client.frames
    val audioFrames: SharedFlow<AudioFrame> = client.audioFrames

    /** A/V sync rendezvous shared between H264Player and OpusPlayer. */
    val avSyncClock = client.avSyncClock

    private val _lastInvalidQr = MutableStateFlow(false)
    val lastInvalidQr: StateFlow<Boolean> = _lastInvalidQr.asStateFlow()

    private val _framesReceived = MutableStateFlow(0L)
    /** Total binary video frames the WebSocket has received. UI debug overlay. */
    val framesReceived: StateFlow<Long> = _framesReceived.asStateFlow()

    /** Real-time link metrics for the diagnostic overlay. Updated by
     *  the sliding-window collector below. */
    private val _linkMetrics = MutableStateFlow(LinkMetrics())
    val linkMetrics: StateFlow<LinkMetrics> = _linkMetrics.asStateFlow()

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

        // Periodic Ping for the diagnostic overlay's RTT readout. Only
        // fires while we have a live Connected state; sleeps cheaply
        // otherwise. 2 s cadence is a compromise between fresh
        // readings and not spamming the WS during a file send.
        viewModelScope.launch {
            while (isActive) {
                if (client.state.value is ConnectionState.Connected) {
                    client.sendPing()
                }
                delay(2_000)
            }
        }

        // 250 ms metrics loop: sliding 1-second window over the
        // per-message byte / frame counters to derive fps + bitrate.
        // The collector itself keeps the deque so the algorithm is
        // self-contained.
        viewModelScope.launch {
            val collector = LinkMetricsCollector(client)
            while (isActive) {
                _linkMetrics.value = collector.tick()
                delay(250)
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

    // M6: file upload (phone → PC).
    val fileEvents = client.fileEvents

    /** Snapshot of every transfer the user has started in this session,
     *  newest first. UI renders these as a stack of progress cards. */
    private val _uploads = MutableStateFlow<List<UploadStatus>>(emptyList())
    val uploads: StateFlow<List<UploadStatus>> = _uploads.asStateFlow()

    init {
        // Single subscriber on `client.fileEvents` so all events flow
        // through one funnel and the per-id `UploadStatus` stays
        // consistent. Lives for the ViewModel's lifetime.
        viewModelScope.launch {
            client.fileEvents.collect { event ->
                // M6 v2: PC announcing an incoming file. The
                // ConnectionClient has already opened the destination
                // file before emitting this; we just seed the UI card.
                if (event is FileTransferEvent.Incoming) {
                    _uploads.value = listOf(
                        UploadStatus(
                            id = event.id,
                            name = event.name,
                            totalBytes = event.totalBytes,
                            bytesSent = 0L,
                            state = UploadState.Sending,
                            direction = com.remotecontrol.app.net.TransferDirection.Download,
                            destPath = event.destPath,
                        ),
                    ) + _uploads.value
                }
                _uploads.value = _uploads.value.map { st ->
                    if (st.id != event.id) st else st.applyEvent(event)
                }
                // For terminal events (Complete / Failed), schedule an
                // explicit removal 6 s later. Prior implementation only
                // filtered the list on every *new* event arrival, which
                // works fine while uploads keep flowing but leaves the
                // last finished card on screen indefinitely if no further
                // event ever fires (the typical single-upload case).
                if (event is FileTransferEvent.Complete ||
                    event is FileTransferEvent.Failed
                ) {
                    val id = event.id
                    viewModelScope.launch {
                        delay(6_000L)
                        _uploads.value = _uploads.value.filter { it.id != id }
                    }
                }
            }
        }
    }

    /** User pressed the ✕ on a transfer card. Dispatches based on the
     *  card's `direction` — outbound uploads flip the streamer's
     *  cancel flag (which then sends `FileTransferAbort` to the PC),
     *  inbound downloads close the output file and send back
     *  `FileSendFailed` so the PC stops streaming. No-op for terminal
     *  cards (the cancel button isn't rendered there). */
    fun cancelTransfer(id: Int) {
        val entry = _uploads.value.firstOrNull { it.id == id } ?: return
        when (entry.direction) {
            com.remotecontrol.app.net.TransferDirection.Upload -> client.cancelUpload(id)
            com.remotecontrol.app.net.TransferDirection.Download -> client.cancelDownload(id)
        }
    }

    /** Trigger an upload. Caller supplies a name + size + a closure that
     *  opens a fresh InputStream over the file (typically
     *  `contentResolver.openInputStream(uri)`). */
    fun uploadFile(name: String, size: Long, open: () -> java.io.InputStream): Int {
        val id = client.uploadFile(name, size, open)
        if (id >= 0) {
            // Seed the UI state with a sending entry so the progress
            // card appears immediately, before the first Progress event
            // arrives. Newest-first ordering.
            _uploads.value = listOf(
                UploadStatus(
                    id = id,
                    name = name,
                    totalBytes = size,
                    bytesSent = 0L,
                    state = UploadState.Sending,
                ),
            ) + _uploads.value
        }
        return id
    }

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

/** UI-facing projection of a single in-flight (or recently finished)
 *  transfer. The ViewModel maintains one of these per `transferId` and
 *  prunes terminal entries a few seconds after they finish so the
 *  user has time to read the result. Used for both upload (phone → PC,
 *  M6 v1) and download (PC → phone, M6 v2); the UI key off `direction`
 *  to swap icon and direction label. */
data class UploadStatus(
    val id: Int,
    val name: String,
    val totalBytes: Long,
    val bytesSent: Long,
    val state: UploadState,
    val direction: com.remotecontrol.app.net.TransferDirection =
        com.remotecontrol.app.net.TransferDirection.Upload,
    val destPath: String? = null,
    val errorReason: String? = null,
    /** Wall-clock millis when this entry became Complete/Failed, used
     *  to auto-dismiss the card after a short delay. Null while
     *  Sending. */
    val terminalAtMs: Long? = null,
) {
    fun applyEvent(event: FileTransferEvent): UploadStatus = when (event) {
        is FileTransferEvent.Accepted -> copy(destPath = event.destPath)
        // `Incoming` is only ever the *first* event for a transfer id;
        // by the time we route into here we've already seeded the entry
        // (see the AppViewModel init block), so absorbing it as a no-op
        // is fine. Listed to keep the `when` exhaustive.
        is FileTransferEvent.Incoming -> this
        is FileTransferEvent.Progress -> copy(
            bytesSent = event.bytesSent,
            // The Begin announced `size` is the authoritative total;
            // event.totalBytes echoes it. Prefer ours so an upstream
            // mismatch doesn't reset the progress bar mid-flight.
            totalBytes = if (totalBytes > 0) totalBytes else event.totalBytes,
        )
        is FileTransferEvent.Complete -> copy(
            state = UploadState.Complete,
            destPath = event.destPath,
            bytesSent = totalBytes,
            terminalAtMs = System.currentTimeMillis(),
        )
        is FileTransferEvent.Failed -> copy(
            state = UploadState.Failed,
            errorReason = event.reason,
            terminalAtMs = System.currentTimeMillis(),
        )
    }
}

enum class UploadState { Sending, Complete, Failed }

/** Sliding-1 s-window aggregator over [ConnectionClient]'s cumulative
 *  byte / frame counters, plus passthrough for `lastRttMs` /
 *  `lastVideoFrameTs`. Held by a single coroutine, so no thread
 *  safety on the deque is needed. */
private class LinkMetricsCollector(private val client: ConnectionClient) {
    private data class Sample(val timeMs: Long, val bytes: Long, val frames: Long)
    private val samples = ArrayDeque<Sample>()
    private val windowMs = 1_000L

    fun tick(): LinkMetrics {
        val now = System.currentTimeMillis()
        val bytes = client.receivedBytes.get()
        val frames = client.receivedFrameCount.get()
        samples.addLast(Sample(now, bytes, frames))
        // Trim everything outside the 1-second window. Keep at least
        // one entry so a `(now - oldest)` div doesn't blow up.
        while (samples.size > 1 && now - samples.first().timeMs > windowMs) {
            samples.removeFirst()
        }
        val (fps, mbps) = if (samples.size >= 2) {
            val oldest = samples.first()
            val dtSec = (now - oldest.timeMs) / 1000.0
            if (dtSec > 0) {
                val df = (frames - oldest.frames).toDouble()
                val db = (bytes - oldest.bytes).toDouble()
                // Mbps = bytes * 8 bits/byte / 1_000_000 / seconds
                (df / dtSec).toFloat() to (db * 8.0 / 1_000_000.0 / dtSec).toFloat()
            } else 0f to 0f
        } else 0f to 0f
        val lastFrame = client.lastVideoFrameTs.get()
        val lastFrameAge = if (lastFrame > 0) now - lastFrame else null
        val rtt = client.lastRttMs.get().takeIf { it >= 0 }
        return LinkMetrics(rttMs = rtt, fps = fps, mbps = mbps, lastFrameAgeMs = lastFrameAge)
    }
}

/** Snapshot of link health for the diagnostic overlay. Computed every
 *  ~250 ms from raw counters on [ConnectionClient] over a 1-second
 *  sliding window so brief jitter doesn't make the readings flicker.
 *
 *  Null fields mean "no measurement yet": pre-Pong → `rttMs`, before
 *  the first video frame → `lastFrameAgeMs`. The UI renders these
 *  as "—" rather than 0 so the user can tell missing vs zero. */
data class LinkMetrics(
    val rttMs: Long? = null,
    val fps: Float = 0f,
    val mbps: Float = 0f,
    val lastFrameAgeMs: Long? = null,
)

@Composable
fun <T> StateFlow<T>.collectAsStateSafely(): State<T> = collectAsState()
