package com.remotecontrol.app.video

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import com.remotecontrol.app.model.AudioStreamInfo
import com.remotecontrol.app.net.AudioFrame
import java.nio.ByteBuffer
import java.util.ArrayDeque
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

private const val TAG = "RC/Opus"
private const val MIME = "audio/opus"

/**
 * Hardware Opus decoder + AudioTrack playback. Uses MediaCodec async API to
 * keep input/output draining naturally; output PCM 16-bit interleaved is
 * written straight to AudioTrack which buffers and clocks playback.
 *
 * AudioTrack `MODE_STREAM` already handles A/V sync passively — frames play
 * at the device's PCM clock rate, which becomes the implicit reference for
 * the rest of the pipeline.
 */
class OpusPlayer {

    private var codec: MediaCodec? = null
    private var audioTrack: AudioTrack? = null
    private val availableInputIndices = ArrayDeque<Int>()
    private val pendingFrames = ArrayDeque<AudioFrame>()
    private val lock = ReentrantLock()

    @Volatile var lastError: String? = null
        private set

    fun start(info: AudioStreamInfo): Result<Unit> {
        stop()
        return try {
            val format = MediaFormat.createAudioFormat(MIME, info.sampleRate, info.channels)
            format.setByteBuffer("csd-0", ByteBuffer.wrap(info.csd0))
            format.setByteBuffer("csd-1", ByteBuffer.wrap(info.csd1))
            format.setByteBuffer("csd-2", ByteBuffer.wrap(info.csd2))

            val c = MediaCodec.createDecoderByType(MIME)
            c.setCallback(buildCallback())
            c.configure(format, null, null, 0)
            c.start()
            codec = c

            val channelMask = if (info.channels == 1) {
                AudioFormat.CHANNEL_OUT_MONO
            } else {
                AudioFormat.CHANNEL_OUT_STEREO
            }
            val minBuf = AudioTrack.getMinBufferSize(
                info.sampleRate,
                channelMask,
                AudioFormat.ENCODING_PCM_16BIT,
            ).coerceAtLeast(8192)

            audioTrack = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build(),
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setSampleRate(info.sampleRate)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(channelMask)
                        .build(),
                )
                .setBufferSizeInBytes(minBuf * 2)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()
            audioTrack?.play()

            lastError = null
            Log.i(TAG, "OpusPlayer started: ${info.sampleRate}Hz ${info.channels}ch")
            Result.success(Unit)
        } catch (e: Exception) {
            val msg = "OpusPlayer init failed: ${e.message}"
            Log.e(TAG, msg, e)
            lastError = msg
            stop()
            Result.failure(e)
        }
    }

    fun stop() {
        codec?.let { c ->
            try { c.stop() } catch (_: Exception) {}
            try { c.release() } catch (_: Exception) {}
        }
        codec = null
        audioTrack?.let { t ->
            try { t.stop() } catch (_: Exception) {}
            try { t.release() } catch (_: Exception) {}
        }
        audioTrack = null
        lock.withLock {
            availableInputIndices.clear()
            pendingFrames.clear()
        }
    }

    fun feed(frame: AudioFrame) {
        val c = codec ?: return
        lock.withLock {
            val idx = availableInputIndices.pollFirst()
            if (idx != null) {
                submit(c, idx, frame)
            } else {
                pendingFrames.addLast(frame)
                while (pendingFrames.size > 64) pendingFrames.pollFirst()
            }
        }
    }

    private fun buildCallback(): MediaCodec.Callback = object : MediaCodec.Callback() {
        override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
            lock.withLock {
                val frame = pendingFrames.pollFirst()
                if (frame != null) {
                    submit(codec, index, frame)
                } else {
                    availableInputIndices.addLast(index)
                }
            }
        }

        override fun onOutputBufferAvailable(
            codec: MediaCodec,
            index: Int,
            info: MediaCodec.BufferInfo,
        ) {
            try {
                val track = audioTrack
                val outBuf = codec.getOutputBuffer(index)
                if (outBuf != null && track != null && info.size > 0) {
                    outBuf.position(info.offset)
                    outBuf.limit(info.offset + info.size)
                    track.write(outBuf, info.size, AudioTrack.WRITE_BLOCKING)
                }
                codec.releaseOutputBuffer(index, false)
            } catch (e: Exception) {
                Log.w(TAG, "audio output handling", e)
            }
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            Log.w(TAG, "audio codec error: ${e.diagnosticInfo}", e)
            lastError = e.diagnosticInfo
        }

        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
            Log.i(TAG, "audio output format changed: $format")
        }
    }

    private fun submit(codec: MediaCodec, index: Int, frame: AudioFrame) {
        try {
            val buf = codec.getInputBuffer(index) ?: return
            buf.clear()
            buf.put(frame.payload)
            codec.queueInputBuffer(index, 0, frame.payload.size, frame.ptsUs, 0)
        } catch (e: Exception) {
            Log.w(TAG, "queueInputBuffer audio", e)
        }
    }
}
