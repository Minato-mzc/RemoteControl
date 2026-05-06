package com.remotecontrol.app.net

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import okio.ByteString

const val PROTOCOL_VERSION = 6

val ProtoJson = Json {
    ignoreUnknownKeys = true
    classDiscriminator = "type"
    encodeDefaults = true
}

// ---------- Client -> Server ----------

@Serializable
sealed interface ClientMsg

@Serializable
@SerialName("hello")
data class Hello(
    val v: Int = PROTOCOL_VERSION,
    val c: String,
    val nonce: String,
    val client: ClientInfo,
) : ClientMsg

@Serializable
@SerialName("ping")
data class Ping(val ts: Long) : ClientMsg

@Serializable
@SerialName("stream_request")
data class StreamRequest(
    val codec: String = "h264",
    @SerialName("max_bitrate_kbps") val maxBitrateKbps: Int? = null,
    @SerialName("max_fps") val maxFps: Int? = null,
    @SerialName("prefer_keyframe_interval_ms") val preferKeyframeIntervalMs: Int? = null,
) : ClientMsg

@Serializable
@SerialName("stream_stop")
data class StreamStop(@SerialName("stream_id") val streamId: String? = null) : ClientMsg

@Serializable
@SerialName("keyframe_request")
data class KeyframeRequest(@SerialName("stream_id") val streamId: String? = null) : ClientMsg

// ---- M3 mouse input ----

/** Mouse position normalized to [0.0, 1.0]. */
@Serializable
@SerialName("mouse_move")
data class MouseMove(val x: Float, val y: Float) : ClientMsg

@Serializable
@SerialName("mouse_button")
data class MouseButton(val button: String, val down: Boolean) : ClientMsg

/** Wheel deltas in notches; positive = up/right. */
@Serializable
@SerialName("mouse_scroll")
data class MouseScroll(val dx: Int = 0, val dy: Int = 0) : ClientMsg

enum class MouseBtn(val wire: String) {
    Left("left"), Right("right"), Middle("middle"),
}

// ---- M3.5 keyboard input ----

/** Inject Unicode text (works for CJK; bypasses PC IME). */
@Serializable
@SerialName("key_text")
data class KeyText(val text: String) : ClientMsg

/** Win32 virtual-key down/up. See VKey for common codes. */
@Serializable
@SerialName("key_event")
data class KeyEvent(val vk: Int, val down: Boolean) : ClientMsg

// ---- M8 clipboard ----

@Serializable
@SerialName("clipboard_set")
data class ClipboardSet(val text: String) : ClientMsg

@Serializable
@SerialName("clipboard_get")
class ClipboardGet : ClientMsg {
    // Empty payload data class can't be a `data class` without fields, use object-like.
    override fun equals(other: Any?): Boolean = other is ClipboardGet
    override fun hashCode(): Int = 0
}

/** Win32 VK_* constants we expose in the keyboard overlay. */
object VKey {
    const val ESCAPE = 0x1B
    const val TAB = 0x09
    const val BACK = 0x08
    const val RETURN = 0x0D
    const val LEFT = 0x25
    const val UP = 0x26
    const val RIGHT = 0x27
    const val DOWN = 0x28
    const val HOME = 0x24
    const val END = 0x23
    const val PRIOR = 0x21 // PageUp
    const val NEXT = 0x22 // PageDown
    const val SNAPSHOT = 0x2C // PrtSc
    const val LWIN = 0x5B
    fun f(n: Int): Int = 0x70 + (n - 1) // F1=0x70 ... F12=0x7B
}

@Serializable
data class ClientInfo(
    val name: String,
    val os: String,
    @SerialName("app_version") val appVersion: String,
)

// ---------- Server -> Client ----------

@Serializable
sealed interface ServerMsg

@Serializable
@SerialName("welcome")
data class Welcome(
    val session: String,
    val server: ServerInfo,
    val hmac: String,
) : ServerMsg

@Serializable
@SerialName("error")
data class ServerError(
    val code: String,
    val msg: String,
) : ServerMsg

@Serializable
@SerialName("pong")
data class Pong(val ts: Long) : ServerMsg

@Serializable
@SerialName("stream_started")
data class StreamStarted(
    @SerialName("stream_id") val streamId: String,
    val codec: String,
    val profile: String,
    val level: String,
    val width: Int,
    val height: Int,
    val fps: Int,
    @SerialName("bitrate_kbps") val bitrateKbps: Int,
    @SerialName("keyframe_interval_frames") val keyframeIntervalFrames: Int,
    @SerialName("pixel_format") val pixelFormat: String,
    @SerialName("started_at_unix_ms") val startedAtUnixMs: Long,
    val audio: AudioMetadataDto? = null,
) : ServerMsg

@Serializable
data class AudioMetadataDto(
    val codec: String,
    @SerialName("sample_rate") val sampleRate: Int,
    val channels: Int,
    @SerialName("frame_size_ms") val frameSizeMs: Int,
    @SerialName("bitrate_kbps") val bitrateKbps: Int,
    @SerialName("csd_0_b64") val csd0B64: String,
    @SerialName("csd_1_b64") val csd1B64: String,
    @SerialName("csd_2_b64") val csd2B64: String,
)

@Serializable
@SerialName("stream_stopped")
data class StreamStopped(
    @SerialName("stream_id") val streamId: String,
    val reason: String,
    val msg: String = "",
) : ServerMsg

@Serializable
@SerialName("clipboard_text")
data class ClipboardText(val text: String) : ServerMsg

@Serializable
data class ServerInfo(
    val name: String,
    val os: String,
    val version: String,
)

// ---------- Binary video frame (data plane) ----------
//
// Wire format (see docs/PROTOCOL.md §"数据平面"):
//   byte 0   : frame_type (0x01 = video)
//   byte 1   : flags     (bit0=keyframe, bit1=config-inlined SPS/PPS)
//   bytes 2-3: reserved (must be 0)
//   bytes 4-11: pts_us (LE u64)
//   bytes 12+: payload — H.264 Annex-B NAL units

object FrameType {
    const val VIDEO = 0x01
    const val AUDIO = 0x02 // reserved for M5
}

object FrameFlags {
    const val KEYFRAME = 0x01
    const val CONFIG = 0x02
}

const val FRAME_HEADER_LEN = 12

/** One Opus packet handed to MediaCodec. PTS is on the same monotonic clock as video. */
data class AudioFrame(
    val payload: ByteArray,
    val ptsUs: Long,
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is AudioFrame) return false
        return ptsUs == other.ptsUs && payload.contentEquals(other.payload)
    }
    override fun hashCode(): Int = (ptsUs.hashCode() * 31 + payload.contentHashCode())
}

data class VideoFrame(
    val type: Int,
    val isKeyframe: Boolean,
    val hasConfig: Boolean,
    val ptsUs: Long,
    val payload: ByteArray,
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is VideoFrame) return false
        return type == other.type && isKeyframe == other.isKeyframe &&
                hasConfig == other.hasConfig && ptsUs == other.ptsUs &&
                payload.contentEquals(other.payload)
    }
    override fun hashCode(): Int = (((type * 31 + ptsUs.hashCode()) * 31 +
            payload.contentHashCode()))
}

object FrameParser {
    /**
     * Parse one binary WebSocket frame into a VideoFrame, or return null if
     * malformed (too short, unknown type, reserved bits non-zero — server bug).
     */
    fun parse(bytes: ByteString): VideoFrame? {
        if (bytes.size < FRAME_HEADER_LEN) return null
        val type = bytes[0].toInt() and 0xFF
        val flags = bytes[1].toInt() and 0xFF
        // bytes 2-3 reserved; we don't validate (forward-compat).
        val ptsUs = readLongLe(bytes, 4)
        val payload = bytes.substring(FRAME_HEADER_LEN).toByteArray()
        return VideoFrame(
            type = type,
            isKeyframe = (flags and FrameFlags.KEYFRAME) != 0,
            hasConfig = (flags and FrameFlags.CONFIG) != 0,
            ptsUs = ptsUs,
            payload = payload,
        )
    }

    private fun readLongLe(b: ByteString, off: Int): Long {
        var v = 0L
        for (i in 0 until 8) {
            v = v or ((b[off + i].toLong() and 0xFFL) shl (i * 8))
        }
        return v
    }
}
