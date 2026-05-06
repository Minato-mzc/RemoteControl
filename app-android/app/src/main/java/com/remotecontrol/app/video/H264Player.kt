package com.remotecontrol.app.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import com.remotecontrol.app.net.VideoFrame
import java.util.ArrayDeque
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

private const val TAG = "RC/Player"

/** Map a server-reported codec name to an Android MediaCodec mime type. */
fun videoMimeFor(codecName: String): String = when (codecName.lowercase()) {
    "hevc", "h265" -> "video/hevc"
    else -> "video/avc"
}

/**
 * Hardware H.264 decoder rendering directly to a Surface. Uses [MediaCodec]'s
 * async Callback API so input/output drain naturally.
 *
 * Frames before the first IDR are dropped (decoder needs SPS/PPS+IDR to start).
 * Pending queue capped at 32 frames; overflow drops oldest — better to glitch
 * than block the WebSocket reader.
 *
 * `start` returns Result so a codec init failure doesn't crash the app —
 * historically MediaCodec configure/start has been one of the easier ways to
 * SIGSEGV the process if the format is even slightly off, so we keep the
 * format minimal (no LOW_LATENCY / COLOR_FORMAT hints) and surface failures.
 */
class H264Player {

    private var codec: MediaCodec? = null
    private val availableInputIndices = ArrayDeque<Int>()
    private val pendingFrames = ArrayDeque<VideoFrame>()
    private val lock = ReentrantLock()
    private var sawKeyframe = false

    // PTS-based render scheduling. Set on first decoded output so subsequent
    // frames render at `clockOriginNs + pts_ns`. Uses the server's monotonic
    // clock as the shared time base with audio.
    @Volatile private var clockOriginNs: Long = 0L
    /** Originally added 150ms to give AudioTrack buffer time, but that left
     *  video noticeably behind audio. Audio's natural buffering already pads
     *  enough; render video on schedule. */
    private val startupLatencyNs: Long = 0L

    @Volatile var lastError: String? = null
        private set

    fun start(surface: Surface, mime: String, width: Int, height: Int): Result<Unit> {
        stop()
        Log.i(TAG, "start $mime ${width}x${height}")
        clockOriginNs = 0L
        // Minimal format. Surface output mode doesn't need COLOR_FORMAT;
        // KEY_LOW_LATENCY occasionally trips drivers on older devices and the
        // gain is small for our use case.
        val format = MediaFormat.createVideoFormat(mime, width, height)
        return try {
            val c = MediaCodec.createDecoderByType(mime)
            c.setCallback(buildCallback())
            c.configure(format, surface, null, 0)
            c.start()
            codec = c
            sawKeyframe = false
            lastError = null
            Result.success(Unit)
        } catch (e: Exception) {
            val msg = "codec init failed: ${e.message}"
            Log.e(TAG, msg, e)
            lastError = msg
            Result.failure(e)
        }
    }

    fun stop() {
        val c = codec ?: return
        codec = null
        sawKeyframe = false
        lock.withLock {
            availableInputIndices.clear()
            pendingFrames.clear()
        }
        try {
            c.stop()
        } catch (_: Exception) { }
        try {
            c.release()
        } catch (_: Exception) { }
    }

    fun feed(frame: VideoFrame) {
        val c = codec ?: return
        lock.withLock {
            if (!sawKeyframe) {
                if (!frame.isKeyframe) return
                sawKeyframe = true
            }
            val idx = availableInputIndices.pollFirst()
            if (idx != null) {
                submit(c, idx, frame)
            } else {
                pendingFrames.addLast(frame)
                while (pendingFrames.size > 32) pendingFrames.pollFirst()
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
                codec.releaseOutputBuffer(index, /* render = */ true)
            } catch (e: Exception) {
                Log.w(TAG, "releaseOutputBuffer", e)
            }
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            val msg = "decoder runtime error: ${e.diagnosticInfo}"
            Log.w(TAG, msg, e)
            lastError = msg
        }

        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
            Log.i(TAG, "output format changed: $format")
        }
    }

    private fun submit(codec: MediaCodec, index: Int, frame: VideoFrame) {
        val buf = codec.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(frame.payload)
        val flags = if (frame.isKeyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
        try {
            codec.queueInputBuffer(index, 0, frame.payload.size, frame.ptsUs, flags)
        } catch (e: Exception) {
            Log.w(TAG, "queueInputBuffer", e)
        }
    }
}
