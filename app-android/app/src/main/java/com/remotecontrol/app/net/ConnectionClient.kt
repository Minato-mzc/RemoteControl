package com.remotecontrol.app.net

import android.os.Build
import android.util.Base64
import android.util.Log
import com.remotecontrol.app.model.ActiveStream
import com.remotecontrol.app.model.AudioStreamInfo
import com.remotecontrol.app.model.ConnectionState
import com.remotecontrol.app.model.QrPayload
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import java.security.SecureRandom
import java.util.concurrent.atomic.AtomicLong
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec
import kotlin.time.Duration.Companion.seconds

private const val TAG = "RC/WS"

/** M6: chunk size for outbound file uploads. 256 KiB hits the sweet
 *  spot — large enough that the per-frame header cost (12 bytes) is
 *  negligible and that fewer than ~100 frames cross the wire per MiB,
 *  small enough that the okhttp send queue doesn't accumulate unbounded
 *  memory while a slow link drains. */
private const val CHUNK_BYTES = 256 * 1024

/** Back-pressure threshold: when okhttp's outbound buffer exceeds this
 *  many bytes the upload loop blocks until the buffer drains below it.
 *
 *  Pairs with [PING_INTERVAL_SECONDS] to keep ping/pong alive during
 *  uploads. OkHttp's WS sender is FIFO and does not prioritize Ping
 *  frames — every Ping enqueued behind a backlog waits the full drain
 *  time before going on the wire. With the threshold at 256 KiB the
 *  queue never carries more than ~512 KiB even transiently, so on any
 *  link faster than ~8 KB/s a Ping drains in <60 s and the keepalive
 *  timer's pong arrives in time. Below 8 KB/s the radio is effectively
 *  unusable for sustained transfers anyway — letting the WS die in
 *  that case is the right signal to the user. */
private const val UPLOAD_BACKPRESSURE_BYTES = 256L * 1024

/** Hard ceiling on how long we'll wait for the okhttp buffer to drain
 *  below [UPLOAD_BACKPRESSURE_BYTES] before giving up. At a 256 KiB
 *  threshold even a 1 Mbps link drains the queue in ~2 s, so a 30 s
 *  stall means the underlying TCP is wedged (radio off, mid-handover,
 *  remote not ACKing) — letting the next `send()` fail surfaces the
 *  fault to the user faster than silently waiting forever. */
private const val UPLOAD_BACKPRESSURE_TIMEOUT_MS = 30_000L

/** Direction of a tracked file transfer. Phone-side cards reuse the
 *  same row UI for both — only the icon and label differ. */
enum class TransferDirection { Upload, Download }

/** Lifecycle events for an in-flight upload OR an inbound download. The
 *  outbound (phone → PC) path emits Accepted/Progress/Complete/Failed.
 *  The inbound (PC → phone) path emits Incoming as the very first event
 *  (instead of Accepted) and then Progress/Complete/Failed identically. */
sealed interface FileTransferEvent {
    val id: Int
    data class Accepted(override val id: Int, val destPath: String) : FileTransferEvent
    data class Incoming(
        override val id: Int,
        val name: String,
        val totalBytes: Long,
        val destPath: String,
    ) : FileTransferEvent
    data class Progress(
        override val id: Int,
        val bytesSent: Long,
        val totalBytes: Long,
    ) : FileTransferEvent
    data class Complete(override val id: Int, val destPath: String) : FileTransferEvent
    data class Failed(override val id: Int, val reason: String) : FileTransferEvent
}

/**
 * Owns a single WebSocket to the PC server. Handles pairing handshake, the
 * stream control sub-protocol, and exposes the inbound binary video frames
 * via [frames] for whoever wants to decode + render them.
 */
class ConnectionClient(
    private val http: OkHttpClient = defaultClient(),
    /** Target dir for inbound files (M6 v2). Passed in by the
     *  ViewModel so this class doesn't have to hold a `Context`. We
     *  ensure it exists on first save. App-private external storage
     *  (`context.getExternalFilesDir("Downloads")`) is the typical
     *  choice — readable by the user via system file managers, but
     *  doesn't need a runtime storage permission. */
    private val downloadsDir: java.io.File? = null,
) {
    private val _state = MutableStateFlow<ConnectionState>(ConnectionState.Idle)
    val state: StateFlow<ConnectionState> = _state.asStateFlow()

    /**
     * Inbound video frames. Backed by a SharedFlow that drops oldest on
     * buffer overflow — losing the occasional frame when the decoder lags is
     * far better than blocking the WebSocket reader.
     *
     * The buffer has to be wide enough to hold a full IDR + a few P-frames
     * for the brief window between `streamStarted` arriving (which is when
     * StreamSurface composes) and the actual `frames.collect` LaunchedEffect
     * starting up. Without it — or with the previous capacity of 8 frames —
     * a fast PC encoder running at 30 fps would emit 8+ frames before
     * Compose finished mounting, the buffer would DROP_OLDEST through the
     * IDR, and the decoder would be stuck waiting for the next GOP boundary
     * (up to 2 s) before it could decode anything. 32 frames at 30 fps
     * gives ~1 s of headroom — comfortable buffer for any reasonable Compose
     * mount, while still bounded so a hung decoder can't OOM us.
     */
    private val _frames = MutableSharedFlow<VideoFrame>(
        replay = 0,
        extraBufferCapacity = 32,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val frames: SharedFlow<VideoFrame> = _frames.asSharedFlow()

    private val _audioFrames = MutableSharedFlow<AudioFrame>(
        replay = 0,
        // Audio buffer slightly larger because frames are 20ms each — at 50fps
        // backpressure shouldn't drop more than a few packets when the decoder
        // is initializing.
        extraBufferCapacity = 32,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val audioFrames: SharedFlow<AudioFrame> = _audioFrames.asSharedFlow()

    /** Cumulative count of binary video frames received, exposed for UI debug overlay. */
    val receivedFrameCount = AtomicLong(0)

    /**
     * A/V sync rendezvous. OpusPlayer publishes the wall-clock + source-PTS
     * of the first audible audio sample here; H264Player reads it on the
     * first decoded frame and aligns its render-clock origin to match, so
     * audio and video for the same source moment land at the same wall
     * time. Lifetime is bound to the client itself; we just `reset()` on
     * each new stream session via [resetAvSyncForNewStream].
     */
    val avSyncClock = com.remotecontrol.app.video.AvSyncClock()

    /** Wipe any previously published audio start time. Called when a fresh
     *  stream session begins so a stale timestamp from the prior session
     *  doesn't anchor the new one. */
    fun resetAvSyncForNewStream() {
        avSyncClock.reset()
    }

    private var webSocket: WebSocket? = null
    private var pendingNonce: ByteArray = ByteArray(0)
    private var pendingKey: ByteArray = ByteArray(0)
    /** Last URL we attempted to connect on — captured so [Listener] can
     *  bundle it into [TrustedServer] when the welcome carries a fresh
     *  trust_token, without re-parsing the URL out of okhttp. */
    private var pendingWsUrl: String = ""

    /** Set on [connect] (QR handshake path), null on [connectTrusted]
     *  (trusted reconnect path). The [Listener] reads it in onOpen to
     *  decide which payload to emit. */
    private var pendingHello: HelloPayload? = null

    /** Optional second-chance dial target — populated from
     *  [QrPayload.fallback] when the scanned QR was the combined LAN+relay
     *  shape. On the FIRST `onFailure` from the primary dial we retry
     *  here exactly once; if that also fails we surface the error
     *  normally. Set to null once consumed so a subsequent failure isn't
     *  silently swallowed. */
    private var pendingFallback: QrPayload? = null
    /** Device name the user chose at scan time. Carried alongside
     *  `pendingFallback` so the retry can reconstruct a fresh
     *  `HelloPayload.Qr` (with its own nonce — the previous one was
     *  consumed when we opened the WS). */
    private var pendingFallbackDeviceName: String = ""

    /** When QR pairing succeeds and the Welcome carries a fresh
     *  trust_token, we drop a [TrustedServer] here so the ViewModel can
     *  persist it. We keep persistence out of the network layer because
     *  it would force ConnectionClient to hold a Context. */
    private val _newlyTrustedServer = MutableStateFlow<TrustedServer?>(null)
    val newlyTrustedServer: StateFlow<TrustedServer?> = _newlyTrustedServer.asStateFlow()

    /** Trusted reconnect was rejected (BadTrustToken / UnknownDevice). The
     *  saved entry is stale — ViewModel listens here and removes it. */
    private val _forgetDeviceId = MutableSharedFlow<String>(
        replay = 0,
        extraBufferCapacity = 1,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val forgetDeviceId: SharedFlow<String> = _forgetDeviceId.asSharedFlow()

    /** Sealed payload picked at connect-time and consumed by Listener.onOpen. */
    private sealed interface HelloPayload {
        data class Qr(
            val code: String,
            val nonce: ByteArray,
            val deviceName: String,
        ) : HelloPayload

        data class Trusted(val server: TrustedServer, val deviceName: String) :
            HelloPayload
    }

    fun connect(payload: QrPayload, deviceName: String = Build.MODEL ?: "Android") {
        disconnect()
        _state.value = ConnectionState.Connecting

        val key = Base64.decode(payload.keyB64Url, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
        val nonce = ByteArray(16).also { SecureRandom().nextBytes(it) }
        pendingKey = key
        pendingNonce = nonce
        pendingWsUrl = payload.wsUrl
        pendingHello = HelloPayload.Qr(payload.code, nonce, deviceName)
        // Stash the relay fallback (if the QR was the combined LAN+relay
        // shape). The Listener.onFailure handler consumes it on the
        // first primary-dial failure so the user gets one automatic
        // retry against the relay before seeing an error screen.
        pendingFallback = payload.fallback
        pendingFallbackDeviceName = deviceName

        val req = Request.Builder().url(payload.wsUrl).build()
        webSocket = http.newWebSocket(req, Listener())
    }

    /**
     * Fast-path reconnect against a previously-paired server. If the ws URL
     * is unreachable (server moved subnets, PC offline) the listener
     * surfaces a Failed state and the UI should fall back to QR scan.
     */
    fun connectTrusted(
        server: TrustedServer,
        deviceName: String = Build.MODEL ?: "Android",
    ) {
        disconnect()
        _state.value = ConnectionState.Connecting

        pendingKey = ByteArray(0)
        pendingNonce = ByteArray(0)
        pendingWsUrl = server.wsUrl
        pendingHello = HelloPayload.Trusted(server, deviceName)

        val req = Request.Builder().url(server.wsUrl).build()
        webSocket = http.newWebSocket(req, Listener())
    }

    fun disconnect() {
        webSocket?.close(1000, "user_disconnect")
        webSocket = null
        _state.value = ConnectionState.Idle
        abortAllIncomingFiles("disconnected")
    }

    /** Ask the server to start a screen stream. Should be called after handshake completes. */
    fun requestStream(
        codec: String = "h264",
        // Plan A: 30fps + 1080p + 30Mbps. After extensive testing of 60fps
        // variants (1080p direct, 720p downscaled, VBR/CBR/AQ/multipass
        // permutations), 60fps either showed mosaic on motion-heavy
        // content (1660Ti throughput cap) or stuttered (WiFi backpressure
        // at higher bitrates). 30fps + 1080p doubles the per-frame budget,
        // gives full native resolution, and has been the only config to
        // deliver clean playback through every combination tested. The
        // tradeoff: cursor / scroll feel slightly choppier than 60fps.
        maxBitrateKbps: Int = 30_000,
        maxFps: Int = 30,
    ) {
        val ws = webSocket ?: return
        val msg = StreamRequest(
            codec = codec,
            maxBitrateKbps = maxBitrateKbps,
            maxFps = maxFps,
            // 1s GOP. NVENC on this rig takes ~1s to ramp bitrate from idle
            // (0.8Mbps) up to motion (25-30Mbps); any P-frame artifacts during
            // that ramp would persist for the whole GOP. With GOP=fps every
            // second of streaming gets a clean refresh, so visible
            // pixelation/blocking self-heals quickly. Bandwidth cost is
            // small (one extra IDR/sec ≈ +20% size vs P-only).
            preferKeyframeIntervalMs = 1000,
        )
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    fun stopStream() {
        val ws = webSocket ?: return
        val st = (_state.value as? ConnectionState.Connected)?.stream ?: return
        val msg = StreamStop(streamId = st.streamId)
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    fun requestKeyframe() {
        val ws = webSocket ?: return
        val st = (_state.value as? ConnectionState.Connected)?.stream ?: return
        val msg = KeyframeRequest(streamId = st.streamId)
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    // ---- M3 mouse input ----

    fun sendMouseMove(xNorm: Float, yNorm: Float) {
        val ws = webSocket ?: return
        val msg = MouseMove(xNorm.coerceIn(0f, 1f), yNorm.coerceIn(0f, 1f))
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    fun sendMouseButton(button: MouseBtn, down: Boolean) {
        val ws = webSocket ?: return
        val msg = MouseButton(button = button.wire, down = down)
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    fun sendMouseScroll(dx: Int = 0, dy: Int = 0) {
        if (dx == 0 && dy == 0) return
        val ws = webSocket ?: return
        val msg = MouseScroll(dx = dx, dy = dy)
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
    }

    // ---- M3.5 keyboard ----

    fun sendKeyText(text: String) {
        if (text.isEmpty()) return
        val ws = webSocket ?: return
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), KeyText(text)))
    }

    fun sendKeyEvent(vk: Int, down: Boolean) {
        val ws = webSocket ?: return
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), KeyEvent(vk, down)))
    }

    /** Convenience: full down + up tap. */
    fun sendKeyTap(vk: Int) {
        sendKeyEvent(vk, true)
        sendKeyEvent(vk, false)
    }

    // ---- M8 clipboard ----

    fun sendClipboardSet(text: String) {
        val ws = webSocket ?: return
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), ClipboardSet(text)))
    }

    fun sendClipboardGet() {
        val ws = webSocket ?: return
        ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), ClipboardGet()))
    }

    /** Hot SharedFlow of clipboard text returned by the PC after `clipboard_get`. */
    private val _clipboardFromPc = MutableSharedFlow<String>(
        replay = 0,
        extraBufferCapacity = 4,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val clipboardFromPc: SharedFlow<String> = _clipboardFromPc.asSharedFlow()

    /** M6: notifications from the file-transfer pipeline. UI subscribes
     *  to display progress / Toast on completion. */
    private val _fileEvents = MutableSharedFlow<FileTransferEvent>(
        replay = 0,
        extraBufferCapacity = 16,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val fileEvents: SharedFlow<FileTransferEvent> = _fileEvents.asSharedFlow()

    /**
     * Stream a file from the phone to the PC. Caller provides:
     *   * `name`  — filename to suggest on the PC side (sanitised server-side).
     *   * `size`  — total byte count for progress UI; server checks against
     *     actual bytes received and warns on mismatch.
     *   * `open`  — opens a fresh `InputStream` over the file. Wrapped in
     *     a closure so the caller doesn't have to hold a file handle
     *     open while we await the WS ack.
     *
     * Flow:
     *   1) Send `FileTransferBegin` with a fresh u32 id.
     *   2) Read the file in 256 KiB chunks; for each, send a binary
     *      frame with `frame_type=FILE`. The final chunk has
     *      `LAST_CHUNK` set.
     *   3) Server emits `FileTransferAccepted` on (1) and
     *      `FileTransferComplete` after the last chunk lands.
     *
     * Returns the transfer id so the UI can correlate events.
     */
    fun uploadFile(
        name: String,
        size: Long,
        open: () -> java.io.InputStream,
    ): Int {
        val ws = webSocket ?: run {
            Log.w(TAG, "uploadFile: no active websocket")
            return -1
        }
        val id = nextTransferId.getAndIncrement()
        ws.send(ProtoJson.encodeToString(
            ClientMsg.serializer(),
            FileTransferBegin(id = id, name = name, size = size),
        ))
        // Stream in a background thread — chunking + WS send must not
        // block whichever main/IO scope called us. We throttle against
        // okhttp's internal send buffer (`queueSize()`) — okhttp closes
        // the WebSocket with code 1001 the moment its buffer exceeds
        // 16 MiB, so naïvely pushing a 1+ GiB file's worth of chunks
        // straight into `send()` blew the connection up at ~16 MiB and
        // dropped the rest on the floor while the UI progress bar
        // happily reported 100 %. Cap the in-flight buffer at ~4 MiB so
        // the connection has plenty of headroom and progress reflects
        // bytes that have actually been pushed onto the wire.
        Thread({
            try {
                open().use { input ->
                    val buf = ByteArray(CHUNK_BYTES)
                    var seq = 0
                    var totalSent = 0L
                    while (true) {
                        val n = input.read(buf)
                        if (n <= 0) {
                            // EOF — send a zero-length terminator with
                            // the LAST flag so the server flushes + closes.
                            val frame = buildFileChunkFrame(
                                transferId = id,
                                chunkSeq = seq,
                                last = true,
                                payload = ByteArray(0),
                            )
                            waitForBufferRoom(ws)
                            if (!ws.send(ByteString.of(*frame))) {
                                throw java.io.IOException(
                                    "WebSocket.send rejected last chunk (buffer/closed)"
                                )
                            }
                            break
                        }
                        val chunk = if (n == buf.size) buf else buf.copyOf(n)
                        val frame = buildFileChunkFrame(
                            transferId = id,
                            chunkSeq = seq,
                            last = false,
                            payload = chunk,
                        )
                        // Back-pressure loop: spin briefly until okhttp's
                        // outbound buffer is small enough to accept us
                        // without risk of tripping the 16 MiB limit.
                        waitForBufferRoom(ws)
                        if (!ws.send(ByteString.of(*frame))) {
                            // `send` only returns false when the socket
                            // is already closed or the buffer is over
                            // 16 MiB. Either way, the rest of the
                            // transfer can't land — bail and let the
                            // server clean up the partial file via its
                            // own peer-disconnect path.
                            throw java.io.IOException(
                                "WebSocket.send rejected (buffer/closed) at seq=$seq"
                            )
                        }
                        seq++
                        totalSent += n
                        _fileEvents.tryEmit(
                            FileTransferEvent.Progress(id, totalSent, size)
                        )
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "uploadFile $id reader error", e)
                ws.send(ProtoJson.encodeToString(
                    ClientMsg.serializer(),
                    FileTransferAbort(id = id, reason = e.message ?: "io"),
                ))
                _fileEvents.tryEmit(FileTransferEvent.Failed(id, e.message ?: "io"))
            }
        }, "RC/Upload-$id").start()
        return id
    }

    private val nextTransferId = java.util.concurrent.atomic.AtomicInteger(1)

    // ---- M6 v2: inbound (PC → phone) file receive state ----

    /** Per-transfer state for an in-flight inbound file. Keyed by the
     *  transfer id the PC announced in `FileSendBegin`. Cleaned up on
     *  LAST_CHUNK, abort, or socket teardown. */
    private data class IncomingFile(
        val id: Int,
        val name: String,
        val totalBytes: Long,
        val dest: java.io.File,
        val out: java.io.BufferedOutputStream,
        var bytesWritten: Long = 0L,
        var nextSeq: Int = 0,
    )

    private val incomingFiles =
        java.util.concurrent.ConcurrentHashMap<Int, IncomingFile>()

    /** Drop every partially-received file on disconnect. Equivalent to
     *  the PC server's `file_transfers.drain()` cleanup — the user
     *  shouldn't be left with half-written files cluttering the
     *  Downloads dir after a flaky connection. */
    private fun abortAllIncomingFiles(reason: String) {
        for ((id, st) in incomingFiles) {
            try {
                st.out.close()
            } catch (_: Exception) {
            }
            if (st.dest.exists() && st.bytesWritten < st.totalBytes) {
                st.dest.delete()
            }
            _fileEvents.tryEmit(FileTransferEvent.Failed(id, reason))
        }
        incomingFiles.clear()
    }

    /** PC asked us to receive `name`. Allocate a destination file under
     *  the app-private Downloads dir, open it, and reply with the
     *  agreed-on path so the PC + UI can show "→ /storage/.../foo.mp4"
     *  on completion. */
    private fun handleFileSendBegin(begin: FileSendBegin) {
        val ws = webSocket
        if (ws == null) {
            Log.w(TAG, "file_send_begin with no live ws — ignoring id=${begin.id}")
            return
        }
        val dir = downloadsDir
        if (dir == null) {
            Log.w(TAG, "file_send_begin but no downloads dir configured — declining id=${begin.id}")
            ws.send(ProtoJson.encodeToString(
                ClientMsg.serializer(),
                FileSendFailed(id = begin.id, reason = "phone has no downloads dir"),
            ))
            return
        }
        if (!dir.exists()) dir.mkdirs()
        val dest = uniqueDestFile(dir, begin.name)
        val out = try {
            java.io.BufferedOutputStream(java.io.FileOutputStream(dest))
        } catch (e: Exception) {
            Log.w(TAG, "open ${dest.path} for inbound id=${begin.id} failed", e)
            ws.send(ProtoJson.encodeToString(
                ClientMsg.serializer(),
                FileSendFailed(id = begin.id, reason = "open: ${e.message ?: e.javaClass.simpleName}"),
            ))
            return
        }
        incomingFiles[begin.id] = IncomingFile(
            id = begin.id,
            name = begin.name,
            totalBytes = begin.size,
            dest = dest,
            out = out,
        )
        ws.send(ProtoJson.encodeToString(
            ClientMsg.serializer(),
            FileSendAccepted(id = begin.id, destPath = dest.absolutePath),
        ))
        _fileEvents.tryEmit(
            FileTransferEvent.Incoming(
                id = begin.id,
                name = begin.name,
                totalBytes = begin.size,
                destPath = dest.absolutePath,
            )
        )
        Log.i(TAG, "incoming file id=${begin.id} ${begin.name} (${begin.size} bytes) → ${dest.path}")
    }

    /** Dispatch one FILE chunk to the matching `IncomingFile`. Called
     *  from `onMessage(Binary)` when `FrameType.FILE` is detected. */
    private fun handleIncomingFileChunk(frame: VideoFrame) {
        // ptsUs layout for FILE: low 32 bits = transfer_id, high 32 bits = chunk_seq
        val transferId = (frame.ptsUs and 0xFFFFFFFFL).toInt()
        val chunkSeq = ((frame.ptsUs ushr 32) and 0xFFFFFFFFL).toInt()
        // `FrameFlags.LAST_CHUNK` shares bit 0 with `FrameFlags.KEYFRAME`,
        // so the generic FrameParser surfaces it via `isKeyframe` for
        // FILE frames. Different semantics, same bit — saves us from
        // needing a separate parser path.
        val isLast = frame.isKeyframe
        val st = incomingFiles[transferId] ?: run {
            Log.w(TAG, "file chunk for unknown id=$transferId")
            return
        }
        val ws = webSocket
        if (chunkSeq != st.nextSeq) {
            Log.w(TAG, "file $transferId out-of-order chunk got=$chunkSeq want=${st.nextSeq}")
            failIncoming(st, "out-of-order chunk $chunkSeq")
            return
        }
        try {
            if (frame.payload.isNotEmpty()) {
                st.out.write(frame.payload)
                st.bytesWritten += frame.payload.size
            }
        } catch (e: Exception) {
            Log.w(TAG, "file $transferId write failed", e)
            failIncoming(st, "write: ${e.message ?: e.javaClass.simpleName}")
            return
        }
        st.nextSeq++
        _fileEvents.tryEmit(
            FileTransferEvent.Progress(transferId, st.bytesWritten, st.totalBytes)
        )
        if (isLast) {
            try {
                st.out.flush()
                st.out.close()
            } catch (e: Exception) {
                Log.w(TAG, "file $transferId close failed", e)
                failIncoming(st, "close: ${e.message ?: e.javaClass.simpleName}")
                return
            }
            incomingFiles.remove(transferId)
            ws?.send(ProtoJson.encodeToString(
                ClientMsg.serializer(),
                FileSendComplete(id = transferId, destPath = st.dest.absolutePath),
            ))
            _fileEvents.tryEmit(
                FileTransferEvent.Complete(transferId, st.dest.absolutePath)
            )
            Log.i(TAG, "incoming file $transferId complete: ${st.bytesWritten} bytes → ${st.dest.path}")
        }
    }

    private fun failIncoming(st: IncomingFile, reason: String) {
        try { st.out.close() } catch (_: Exception) {}
        st.dest.delete()
        incomingFiles.remove(st.id)
        webSocket?.send(ProtoJson.encodeToString(
            ClientMsg.serializer(),
            FileSendFailed(id = st.id, reason = reason),
        ))
        _fileEvents.tryEmit(FileTransferEvent.Failed(st.id, reason))
    }

    /** Pick a path inside `dir` that doesn't already exist. Tries
     *  `name`, then `name (2)`, `name (3)`, ... up to 999. Past that
     *  we just append a UUID and call it a day. */
    private fun uniqueDestFile(dir: java.io.File, name: String): java.io.File {
        val sanitized = name.replace(Regex("""[\\/:*?"<>|]"""), "_")
        val candidate = java.io.File(dir, sanitized)
        if (!candidate.exists()) return candidate
        val dotIdx = sanitized.lastIndexOf('.')
        val (base, ext) = if (dotIdx > 0) {
            sanitized.substring(0, dotIdx) to sanitized.substring(dotIdx)
        } else {
            sanitized to ""
        }
        for (i in 2..999) {
            val alt = java.io.File(dir, "$base ($i)$ext")
            if (!alt.exists()) return alt
        }
        return java.io.File(dir, "$base-${java.util.UUID.randomUUID()}$ext")
    }

    /** Block until okhttp's outbound buffer drops below
     *  [UPLOAD_BACKPRESSURE_BYTES], polling every 20 ms. Caps total wait
     *  so we don't get stuck forever on a fully dead connection — past
     *  the cap we just return and let the caller's `send` fail
     *  (returning false) so the upload aborts loudly instead of hanging.
     */
    private fun waitForBufferRoom(ws: okhttp3.WebSocket) {
        val startMs = System.currentTimeMillis()
        while (ws.queueSize() > UPLOAD_BACKPRESSURE_BYTES) {
            if (System.currentTimeMillis() - startMs > UPLOAD_BACKPRESSURE_TIMEOUT_MS) {
                Log.w(
                    TAG,
                    "waitForBufferRoom: still ${ws.queueSize()} bytes queued after " +
                        "${UPLOAD_BACKPRESSURE_TIMEOUT_MS}ms — giving up",
                )
                return
            }
            try {
                Thread.sleep(20)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
                return
            }
        }
    }

    private inner class Listener : WebSocketListener() {

        override fun onOpen(ws: WebSocket, response: Response) {
            Log.i(TAG, "opened ${response.request.url}")
            val helloPayload = pendingHello
            val msg: ClientMsg = when (helloPayload) {
                is HelloPayload.Qr -> Hello(
                    c = helloPayload.code,
                    nonce = base64Url(helloPayload.nonce),
                    client = ClientInfo(
                        name = helloPayload.deviceName,
                        os = "HarmonyOS/Android API ${Build.VERSION.SDK_INT}",
                        appVersion = "0.1.0",
                    ),
                )
                is HelloPayload.Trusted -> TrustedHello(
                    deviceId = helloPayload.server.deviceId,
                    token = helloPayload.server.token,
                    client = ClientInfo(
                        name = helloPayload.deviceName,
                        os = "HarmonyOS/Android API ${Build.VERSION.SDK_INT}",
                        appVersion = "0.1.0",
                    ),
                )
                null -> {
                    // Race: connect() got cancelled between newWebSocket and
                    // onOpen. Just close.
                    ws.close(1001, "no_pending_hello")
                    return
                }
            }
            ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), msg))
        }

        override fun onMessage(ws: WebSocket, text: String) {
            val msg = try {
                ProtoJson.decodeFromString(ServerMsg.serializer(), text)
            } catch (e: Exception) {
                Log.w(TAG, "bad server msg: $text", e)
                _state.value = ConnectionState.Failed("服务器返回的消息无法解析")
                ws.close(1002, "bad_message")
                return
            }
            when (msg) {
                is Welcome -> handleWelcome(msg, ws)
                is ServerError -> {
                    Log.w(TAG, "server error ${msg.code}: ${msg.msg}")
                    // Trusted-reconnect rejection → tell the ViewModel to
                    // forget the saved entry. The server's view of trust is
                    // authoritative; if it doesn't recognize us, our token
                    // is stale.
                    if (msg.code == "bad_trust_token" || msg.code == "unknown_device") {
                        (pendingHello as? HelloPayload.Trusted)?.let {
                            _forgetDeviceId.tryEmit(it.server.deviceId)
                        }
                    }
                    _state.value = ConnectionState.Failed(mapErrorCode(msg.code, msg.msg))
                    ws.close(1000, "rejected")
                }
                is Pong -> Unit
                is StreamStarted -> handleStreamStarted(msg)
                is StreamStopped -> handleStreamStopped(msg)
                is ClipboardText -> {
                    Log.i(TAG, "clipboard from PC: len=${msg.text.length}")
                    _clipboardFromPc.tryEmit(msg.text)
                }
                is FileTransferAccepted -> {
                    Log.i(TAG, "file ${msg.id} accepted → ${msg.destPath}")
                    _fileEvents.tryEmit(FileTransferEvent.Accepted(msg.id, msg.destPath))
                }
                is FileTransferComplete -> {
                    Log.i(TAG, "file ${msg.id} complete → ${msg.destPath}")
                    _fileEvents.tryEmit(FileTransferEvent.Complete(msg.id, msg.destPath))
                }
                is FileTransferFailed -> {
                    Log.w(TAG, "file ${msg.id} failed: ${msg.reason}")
                    _fileEvents.tryEmit(FileTransferEvent.Failed(msg.id, msg.reason))
                }
                is FileSendBegin -> handleFileSendBegin(msg)
            }
        }

        override fun onMessage(ws: WebSocket, bytes: ByteString) {
            val frame = FrameParser.parse(bytes)
            if (frame == null) {
                Log.w(TAG, "binary frame too short: ${bytes.size} bytes")
                return
            }
            when (frame.type) {
                FrameType.VIDEO -> {
                    receivedFrameCount.incrementAndGet()
                    _frames.tryEmit(frame)
                }
                FrameType.AUDIO -> {
                    _audioFrames.tryEmit(AudioFrame(payload = frame.payload, ptsUs = frame.ptsUs))
                }
                FrameType.FILE -> handleIncomingFileChunk(frame)
                else -> { /* unknown — forward-compat */ }
            }
        }

        override fun onFailure(ws: WebSocket, t: Throwable, response: Response?) {
            Log.w(TAG, "ws failed", t)
            // Stale-listener guard: if `webSocket` no longer points at
            // this `ws`, the user has already started a fresh connection
            // and this callback belongs to the *previous* WS finishing
            // its close handshake. We must NOT touch any shared state
            // (the `webSocket` ref or `_state`) — otherwise we'd clobber
            // the new connection's ws ref to null and the next
            // `requestStream()` would silently return because its
            // `val ws = webSocket ?: return` guard would short-circuit.
            // Symptom observed in production: after manually
            // disconnecting mid-upload and immediately reconnecting,
            // the phone sits on "正在请求屏幕串流" forever because the
            // old upload's lingering Close handshake clobbers the new
            // ws on completion.
            if (webSocket !== ws) {
                Log.i(TAG, "stale onFailure for old ws — ignored")
                return
            }
            abortAllIncomingFiles("ws failed: ${t.message ?: "?"}")
            // Combined-QR fallback: if the scanned QR carried both a LAN
            // primary and a relay backup, consume the backup exactly
            // once and retry the dial against it. Typical use: user is
            // off-LAN (4G / different Wi-Fi), LAN host is unreachable,
            // we silently switch to the relay path so they don't have
            // to scan again or know which network they're on.
            val fb = pendingFallback
            if (fb != null) {
                pendingFallback = null
                Log.i(TAG, "primary dial failed; retrying via relay fallback ${fb.wsUrl}")
                webSocket = null
                // Reuse `connect` so all the per-dial state (key, nonce,
                // pendingHello) gets re-initialised cleanly.
                connect(fb, pendingFallbackDeviceName)
                return
            }
            _state.value = ConnectionState.Failed(t.message ?: "连接失败")
            webSocket = null
        }

        override fun onClosing(ws: WebSocket, code: Int, reason: String) {
            ws.close(1000, null)
        }

        override fun onClosed(ws: WebSocket, code: Int, reason: String) {
            Log.i(TAG, "closed code=$code reason=$reason")
            // Same stale-listener guard as in `onFailure` — see there
            // for the full rationale.
            if (webSocket !== ws) {
                Log.i(TAG, "stale onClosed for old ws — ignored")
                return
            }
            webSocket = null
            if (_state.value is ConnectionState.Connected) {
                _state.value = ConnectionState.Idle
            }
            abortAllIncomingFiles("ws closed: $reason")
        }
    }

    private fun handleWelcome(welcome: Welcome, ws: WebSocket) {
        // QR-pairing path: server proves it knows the key by HMAC-ing our
        // nonce. Trusted-reconnect path: pendingKey is empty, server's HMAC
        // field is empty too — the trust token itself was the proof.
        val isTrustedReconnect = pendingKey.isEmpty() && pendingNonce.isEmpty()
        if (!isTrustedReconnect) {
            val expected = hmacSha256Hex(pendingKey, pendingNonce)
            if (!constantTimeEquals(expected, welcome.hmac)) {
                Log.w(TAG, "HMAC mismatch — possible man-in-the-middle")
                _state.value = ConnectionState.Failed("服务器身份验证失败（HMAC 不匹配）")
                ws.close(1008, "hmac_mismatch")
                return
            }
        }
        // Hand the new token (if any) up to the ViewModel for persistence.
        // Only fires on the QR path — trusted reconnects don't reissue.
        val token = welcome.trustToken
        val deviceId = welcome.deviceId
        if (token != null && deviceId != null) {
            _newlyTrustedServer.value = TrustedServer(
                deviceId = deviceId,
                token = token,
                wsUrl = pendingWsUrl,
                serverName = welcome.server.name,
                lastConnectedMs = System.currentTimeMillis(),
            )
        }
        _state.value = ConnectionState.Connected(
            serverName = welcome.server.name,
            serverOs = welcome.server.os,
            serverVersion = welcome.server.version,
            session = welcome.session,
        )
    }

    private fun handleStreamStarted(msg: StreamStarted) {
        val cur = _state.value as? ConnectionState.Connected ?: return
        // Wipe stale A/V sync data before the new stream's first audio sample
        // arrives. If we left the previous session's value in place,
        // OpusPlayer's idempotent publishAudioStart would silently no-op for
        // this session and H264Player would lock to a wall-clock reference
        // from the *previous* connection — A/V would drift wildly.
        resetAvSyncForNewStream()
        val audio = msg.audio?.let { dto ->
            AudioStreamInfo(
                codec = dto.codec,
                sampleRate = dto.sampleRate,
                channels = dto.channels,
                csd0 = Base64.decode(dto.csd0B64, Base64.DEFAULT),
                csd1 = Base64.decode(dto.csd1B64, Base64.DEFAULT),
                csd2 = Base64.decode(dto.csd2B64, Base64.DEFAULT),
            )
        }
        _state.value = cur.copy(
            stream = ActiveStream(
                streamId = msg.streamId,
                codec = msg.codec,
                width = msg.width,
                height = msg.height,
                fps = msg.fps,
                bitrateKbps = msg.bitrateKbps,
                audio = audio,
            )
        )
        Log.i(
            TAG,
            "stream started: ${msg.width}x${msg.height}@${msg.fps}fps id=${msg.streamId} audio=${audio != null}"
        )
    }

    private fun handleStreamStopped(msg: StreamStopped) {
        val cur = _state.value as? ConnectionState.Connected ?: return
        _state.value = cur.copy(stream = null)
        Log.i(TAG, "stream stopped (${msg.reason}): ${msg.msg}")
    }

    private fun mapErrorCode(code: String, msg: String): String = when (code) {
        "bad_pairing_code" -> "配对码错误"
        "code_expired" -> "配对码已过期，请重新扫码"
        "code_used" -> "该二维码已被使用过"
        "version_mismatch" -> "协议版本不匹配：$msg"
        "malformed" -> "消息格式错误：$msg"
        "stream_unavailable" -> "屏幕串流不可用：$msg"
        "stream_already_running" -> "已有活跃的串流"
        "not_authenticated" -> "未握手，请先重新连接"
        "unknown_device" -> "服务器不认识本设备，请重新扫码配对"
        "bad_trust_token" -> "信任凭证已失效，请重新扫码配对"
        else -> "$code: $msg"
    }

    companion object {
        /** WebSocket keepalive interval. OkHttp uses this same value as
         *  the pong-timeout: if the server doesn't pong within
         *  pingInterval of the ping going out, the WS is torn down as
         *  a dead-link signal.
         *
         *  20 s is a fine number for an idle WS, but it falls apart
         *  during a file upload on a slow cellular link: OkHttp queues
         *  pings behind data frames, and at e.g. 50 KB/s upstream a
         *  256 KiB chunk takes 5 s just to leave the radio — multiple
         *  consecutive slow chunks easily pad the queue past 20 s of
         *  pending bytes, and the next ping starves. 60 s tolerates
         *  brief tower handovers / congestion dips that are routine on
         *  4G without sacrificing too much dead-link detection latency
         *  (we'll still notice a truly dead radio inside a minute). */
        private const val PING_INTERVAL_SECONDS = 60L

        private fun defaultClient(): OkHttpClient = OkHttpClient.Builder()
            .pingInterval(PING_INTERVAL_SECONDS, java.util.concurrent.TimeUnit.SECONDS)
            // Connect timeout 4s (default 10s) so a combined-QR scan
            // doesn't hang for a full 10 seconds when the LAN address
            // is unreachable. The phone-side relay fallback in
            // `onFailure` then kicks in within ~4s instead of ~10s.
            // 4s is comfortably more than typical LAN dial RTT (sub-50ms
            // intra-router) plus DNS, so we don't false-trip on healthy
            // connections.
            .connectTimeout(4, java.util.concurrent.TimeUnit.SECONDS)
            .build()

        private fun base64Url(bytes: ByteArray): String =
            Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)

        private fun hmacSha256Hex(key: ByteArray, data: ByteArray): String {
            val mac = Mac.getInstance("HmacSHA256")
            mac.init(SecretKeySpec(key, "HmacSHA256"))
            val out = mac.doFinal(data)
            val sb = StringBuilder(out.size * 2)
            for (b in out) sb.append("%02x".format(b))
            return sb.toString()
        }

        private fun constantTimeEquals(a: String, b: String): Boolean {
            if (a.length != b.length) return false
            var diff = 0
            for (i in a.indices) diff = diff or (a[i].code xor b[i].code)
            return diff == 0
        }
    }
}
