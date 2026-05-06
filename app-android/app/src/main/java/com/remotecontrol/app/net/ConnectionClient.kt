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
     */
    private val _frames = MutableSharedFlow<VideoFrame>(
        replay = 0,
        extraBufferCapacity = 8,
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

    private var webSocket: WebSocket? = null
    private var pendingNonce: ByteArray = ByteArray(0)
    private var pendingKey: ByteArray = ByteArray(0)

    fun connect(payload: QrPayload, deviceName: String = Build.MODEL ?: "Android") {
        disconnect()
        _state.value = ConnectionState.Connecting

        val key = Base64.decode(payload.keyB64Url, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
        val nonce = ByteArray(16).also { SecureRandom().nextBytes(it) }
        pendingKey = key
        pendingNonce = nonce

        val req = Request.Builder().url(payload.wsUrl).build()
        webSocket = http.newWebSocket(req, Listener(payload.code, nonce, deviceName))
    }

    fun disconnect() {
        webSocket?.close(1000, "user_disconnect")
        webSocket = null
        _state.value = ConnectionState.Idle
    }

    /** Ask the server to start a screen stream. Should be called after handshake completes. */
    fun requestStream(
        codec: String = "h264",
        maxBitrateKbps: Int = 30_000,
        maxFps: Int = 30,
    ) {
        val ws = webSocket ?: return
        val msg = StreamRequest(
            codec = codec,
            maxBitrateKbps = maxBitrateKbps,
            maxFps = maxFps,
            // 3s GOP — fewer IDR frames means more bits go to P frames,
            // helping motion-heavy content (videos, scrolling) stay sharp.
            preferKeyframeIntervalMs = 3000,
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

    private inner class Listener(
        private val pairingCode: String,
        private val nonce: ByteArray,
        private val deviceName: String,
    ) : WebSocketListener() {

        override fun onOpen(ws: WebSocket, response: Response) {
            Log.i(TAG, "opened ${response.request.url}")
            val hello = Hello(
                c = pairingCode,
                nonce = base64Url(nonce),
                client = ClientInfo(
                    name = deviceName,
                    os = "HarmonyOS/Android API ${Build.VERSION.SDK_INT}",
                    appVersion = "0.1.0",
                ),
            )
            ws.send(ProtoJson.encodeToString(ClientMsg.serializer(), hello))
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
        val expected = hmacSha256Hex(pendingKey, pendingNonce)
        if (!constantTimeEquals(expected, welcome.hmac)) {
            Log.w(TAG, "HMAC mismatch — possible man-in-the-middle")
            _state.value = ConnectionState.Failed("服务器身份验证失败（HMAC 不匹配）")
            ws.close(1008, "hmac_mismatch")
            return
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
