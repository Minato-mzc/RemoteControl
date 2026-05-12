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

/**
 * Skip-the-QR reconnect handshake. Sent in place of [Hello] when we
 * already have a `(deviceId, token)` minted from a previous successful
 * pairing on this server. Server verifies against its trusted-devices
 * file; on match we get back the same Welcome shape (minus the new
 * trust_token / device_id fields, which we're already holding).
 */
@Serializable
@SerialName("trusted_hello")
data class TrustedHello(
    val v: Int = PROTOCOL_VERSION,
    @SerialName("device_id") val deviceId: String,
    val token: String,
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

// ---- M6 file transfer (phone → PC) ----

@Serializable
@SerialName("file_transfer_begin")
data class FileTransferBegin(
    val id: Int,
    val name: String,
    val size: Long,
) : ClientMsg

@Serializable
@SerialName("file_transfer_abort")
data class FileTransferAbort(
    val id: Int,
    val reason: String = "",
) : ClientMsg

// ---- M6 v2 file transfer (PC → phone). Phone is the receiver. ----

@Serializable
@SerialName("file_send_accepted")
data class FileSendAccepted(
    val id: Int,
    @SerialName("dest_path") val destPath: String,
) : ClientMsg

@Serializable
@SerialName("file_send_complete")
data class FileSendComplete(
    val id: Int,
    @SerialName("dest_path") val destPath: String,
) : ClientMsg

@Serializable
@SerialName("file_send_failed")
data class FileSendFailed(
    val id: Int,
    val reason: String,
) : ClientMsg

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
    const val CONTROL = 0x11
    const val MENU = 0x12 // Alt
    const val SHIFT = 0x10
    const val DELETE = 0x2E
    fun f(n: Int): Int = 0x70 + (n - 1) // F1=0x70 ... F12=0x7B
    /** Letter VK code: 'A' (0x41) ... 'Z' (0x5A). Pass uppercase ASCII. */
    fun letter(c: Char): Int = c.uppercaseChar().code
}

/** Steps inside a [Macro], played sequentially on the server. */
sealed interface MacroStep {
    data class KeyDown(val vk: Int) : MacroStep
    data class KeyUp(val vk: Int) : MacroStep
    /** Convenience: down + up. */
    data class KeyTap(val vk: Int) : MacroStep
    /** Inter-step delay so the PC OS observes events as separate keystrokes. */
    data class Delay(val ms: Long) : MacroStep
}

data class Macro(val label: String, val steps: List<MacroStep>)

/** Built-in shortcut macros (M7 v1). User-defined macros come later. */
object Macros {
    private fun combo(modifier: Int, vk: Int) = listOf(
        MacroStep.KeyDown(modifier),
        MacroStep.Delay(8),
        MacroStep.KeyTap(vk),
        MacroStep.Delay(8),
        MacroStep.KeyUp(modifier),
    )

    val CTRL_C = Macro("Ctrl+C", combo(VKey.CONTROL, VKey.letter('C')))
    val CTRL_V = Macro("Ctrl+V", combo(VKey.CONTROL, VKey.letter('V')))
    val CTRL_X = Macro("Ctrl+X", combo(VKey.CONTROL, VKey.letter('X')))
    val CTRL_A = Macro("Ctrl+A", combo(VKey.CONTROL, VKey.letter('A')))
    val CTRL_Z = Macro("Ctrl+Z", combo(VKey.CONTROL, VKey.letter('Z')))
    val ALT_TAB = Macro("Alt+Tab", combo(VKey.MENU, VKey.TAB))
    val WIN_R = Macro("Win+R", combo(VKey.LWIN, VKey.letter('R')))
    val WIN_D = Macro("Win+D", combo(VKey.LWIN, VKey.letter('D')))

    val DEFAULTS: List<Macro> = listOf(CTRL_C, CTRL_V, CTRL_X, CTRL_A, CTRL_Z, ALT_TAB, WIN_R, WIN_D)
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
    /** Long-lived token to persist for [TrustedHello] reconnects. Null
     *  when this Welcome is itself the response to a TrustedHello (the
     *  client already has the token). */
    @SerialName("trust_token") val trustToken: String? = null,
    /** Stable id the server assigned to this device. Echo back in
     *  [TrustedHello.deviceId]. Null on TrustedHello replies. */
    @SerialName("device_id") val deviceId: String? = null,
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

// ---- M6 server replies ----

@Serializable
@SerialName("file_transfer_accepted")
data class FileTransferAccepted(
    val id: Int,
    @SerialName("dest_path") val destPath: String,
) : ServerMsg

@Serializable
@SerialName("file_transfer_complete")
data class FileTransferComplete(
    val id: Int,
    @SerialName("dest_path") val destPath: String,
) : ServerMsg

@Serializable
@SerialName("file_transfer_failed")
data class FileTransferFailed(
    val id: Int,
    val reason: String,
) : ServerMsg

// ---- M6 v2 inbound announcement from PC ----

@Serializable
@SerialName("file_send_begin")
data class FileSendBegin(
    val id: Int,
    val name: String,
    val size: Long,
) : ServerMsg

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
    /** M6 file-transfer chunk. Reuses the 12-byte header layout; the
     *  8 PTS bytes are repurposed as
     *  `transfer_id (LE u32) || chunk_seq (LE u32)`. */
    const val FILE = 0x03
}

object FrameFlags {
    const val KEYFRAME = 0x01
    const val CONFIG = 0x02
    /** M6 file-transfer: marks the FINAL chunk in a transfer. */
    const val LAST_CHUNK = 0x01
}

/**
 * Build a single file-chunk WS Binary frame. Header is 12 bytes; layout
 * mirrors the spec:
 *   byte 0     : frame_type = 0x03
 *   byte 1     : flags     (bit0 = LAST_CHUNK)
 *   bytes 2-3  : reserved (must be 0)
 *   bytes 4-7  : transfer_id (LE u32)
 *   bytes 8-11 : chunk_seq   (LE u32)
 *   bytes 12+  : payload
 *
 * The split of the 8 PTS bytes into two LE u32 fields keeps the header
 * size the same as video / audio frames so transports that already know
 * how to relay a 12-byte-prefixed binary frame don't need any changes.
 */
fun buildFileChunkFrame(
    transferId: Int,
    chunkSeq: Int,
    last: Boolean,
    payload: ByteArray,
): ByteArray {
    val out = ByteArray(FRAME_HEADER_LEN + payload.size)
    out[0] = FrameType.FILE.toByte()
    out[1] = (if (last) FrameFlags.LAST_CHUNK else 0).toByte()
    // bytes 2,3: reserved (0)
    // transfer_id LE u32 at bytes 4-7
    out[4] = (transferId and 0xFF).toByte()
    out[5] = ((transferId ushr 8) and 0xFF).toByte()
    out[6] = ((transferId ushr 16) and 0xFF).toByte()
    out[7] = ((transferId ushr 24) and 0xFF).toByte()
    // chunk_seq LE u32 at bytes 8-11
    out[8] = (chunkSeq and 0xFF).toByte()
    out[9] = ((chunkSeq ushr 8) and 0xFF).toByte()
    out[10] = ((chunkSeq ushr 16) and 0xFF).toByte()
    out[11] = ((chunkSeq ushr 24) and 0xFF).toByte()
    System.arraycopy(payload, 0, out, FRAME_HEADER_LEN, payload.size)
    return out
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
