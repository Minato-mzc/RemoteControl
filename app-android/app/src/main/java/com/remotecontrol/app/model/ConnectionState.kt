package com.remotecontrol.app.model

sealed interface ConnectionState {
    data object Idle : ConnectionState
    data object Connecting : ConnectionState
    data class Connected(
        val serverName: String,
        val serverOs: String,
        val serverVersion: String,
        val session: String,
        val stream: ActiveStream? = null,
    ) : ConnectionState
    data class Failed(val reason: String) : ConnectionState
}

/** Currently-running screen stream metadata, attached to [ConnectionState.Connected] when active. */
data class ActiveStream(
    val streamId: String,
    val codec: String,
    val width: Int,
    val height: Int,
    val fps: Int,
    val bitrateKbps: Int,
    val audio: AudioStreamInfo? = null,
)

/** Opus audio sub-stream info, decoded from server's csd_*_b64 fields. */
data class AudioStreamInfo(
    val codec: String,
    val sampleRate: Int,
    val channels: Int,
    /** csd-0: Opus ID Header bytes. */
    val csd0: ByteArray,
    /** csd-1: pre-skip nanoseconds, LE i64. */
    val csd1: ByteArray,
    /** csd-2: seek pre-roll nanoseconds, LE i64. */
    val csd2: ByteArray,
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is AudioStreamInfo) return false
        return codec == other.codec && sampleRate == other.sampleRate &&
                channels == other.channels && csd0.contentEquals(other.csd0) &&
                csd1.contentEquals(other.csd1) && csd2.contentEquals(other.csd2)
    }
    override fun hashCode(): Int =
        ((codec.hashCode() * 31 + sampleRate) * 31 + channels) * 31 +
                csd0.contentHashCode()
}
