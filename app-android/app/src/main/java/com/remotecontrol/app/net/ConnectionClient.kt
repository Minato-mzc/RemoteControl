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

/**
 * Owns a single WebSocket to the PC server. Handles pairing handshake, the
 * stream control sub-protocol, and exposes the inbound binary video frames
 * via [frames] for whoever wants to decode + render them.
 */
class ConnectionClient(
    private val http: OkHttpClient = defaultClient(),
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
                else -> { /* unknown — forward-compat */ }
            }
        }

        override fun onFailure(ws: WebSocket, t: Throwable, response: Response?) {
            Log.w(TAG, "ws failed", t)
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
            webSocket = null
            if (_state.value is ConnectionState.Connected) {
                _state.value = ConnectionState.Idle
            }
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
        private fun defaultClient(): OkHttpClient = OkHttpClient.Builder()
            .pingInterval(20.seconds.inWholeMilliseconds, java.util.concurrent.TimeUnit.MILLISECONDS)
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
