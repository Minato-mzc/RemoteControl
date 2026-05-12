package com.remotecontrol.app.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
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
    /** Fallback startup latency when no audio reference is available
     *  (audio sub-stream disabled, or video decoded faster than the audio
     *  pipeline). Used only when [avSync] returns null on the first
     *  frame. With a working audio reference the video clock origin is
     *  computed against the audio's wall-clock timestamp instead, which
     *  is the true source of A/V drift on Android. */
    private val standaloneStartupLatencyNs: Long = 100_000_000L // 100 ms

    /** Optional A/V sync rendezvous. When set, the first decoded frame's
     *  clock origin is anchored to the audio path's published reference
     *  so video PTS X lands at the same wall clock as audio PTS X. */
    private var avSync: AvSyncClock? = null
    /** Tracks whether the current `clockOriginNs` was computed from
     *  [avSync] or from the standalone fallback. */
    @Volatile private var anchoredOnAudio: Boolean = false
    /** Wall-ns of the audio reference we last anchored against. Used to
     *  detect when the audio path has refined its published value (seed
     *  → getTimestamp-derived) so we re-anchor exactly once on the
     *  upgrade and not repeatedly on noise. */
    @Volatile private var lastAnchoredAudioWallNs: Long = 0L

    /** Background thread used to *manually* delay
     *  `releaseOutputBuffer(idx, true)` calls. We have to do this
     *  ourselves on devices where the codec ignores the
     *  `releaseOutputBuffer(idx, renderTimeNs)` schedule and just
     *  renders ASAP — measured on a HiSi/PLR-AL30 to be ~400 ms early
     *  on every frame, completely defeating A/V sync. The handler
     *  thread is short-lived (recreated per `start()`) so we don't
     *  leak it across stream sessions. */
    private var renderThread: HandlerThread? = null
    private var renderHandler: Handler? = null
    /** Maximum time we'll hold an output buffer waiting for its render
     *  slot. If we held longer, MediaCodec runs out of output slots and
     *  starves the input pipeline. 400 ms is a comfortable cap for
     *  matching AudioTrack pre-roll on most devices while still
     *  leaving the codec multiple frames of breathing room (output
     *  pool is typically 8+ buffers and we run at 30 fps). */
    private val maxRenderDelayMs: Long = 400L

    @Volatile var lastError: String? = null
        private set

    fun start(
        surface: Surface,
        mime: String,
        width: Int,
        height: Int,
        avSync: AvSyncClock? = null,
    ): Result<Unit> {
        stop()
        Log.i(TAG, "start $mime ${width}x${height}")
        this.avSync = avSync
        clockOriginNs = 0L
        // Spin up a fresh handler thread for delayed releases. This
        // runs separately from MediaCodec's own callback HandlerThread
        // so a delayed release doesn't block the codec's internal
        // dispatch loop.
        val ht = HandlerThread("RC/Player-render").also { it.start() }
        renderThread = ht
        renderHandler = Handler(ht.looper)
        // Configure the codec, optionally with `KEY_LOW_LATENCY` on
        // API 30+. On PLR-AL30 (HarmonyOS, HiSilicon) the default
        // configure path swallows ~300 input frames before producing
        // output #1, costing ~10 s of black screen at every reconnect.
        // Low-latency cuts that to under a second on devices that
        // honour the flag — but the vendor decoder on some Huawei
        // builds reports API 30 yet rejects the option with a generic
        // `0x80001001` from `configure()`, so we try the low-latency
        // path first and silently fall back to the legacy format
        // without retrying any other settings.
        fun buildFormat(lowLatency: Boolean): MediaFormat {
            val f = MediaFormat.createVideoFormat(mime, width, height)
            // `KEY_OPERATING_RATE = INT_MAX` and `KEY_PRIORITY = 0`
            // request realtime / max-clock from the decoder up front.
            // On HiSilicon (HarmonyOS) this is what actually shrinks
            // the configure→first-output gap from ~10 s to ~1 s — the
            // vendor decoder otherwise idles in a low-power state and
            // takes ages to clock up to real-time work. Both keys are
            // standard since API 23 (operating rate) / API 23
            // (priority), so no version gate is needed.
            f.setInteger(MediaFormat.KEY_OPERATING_RATE, Int.MAX_VALUE)
            f.setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (
                lowLatency &&
                android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R
            ) {
                f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
            return f
        }
        fun configureCodec(lowLatency: Boolean): MediaCodec {
            val c = MediaCodec.createDecoderByType(mime)
            c.setCallback(buildCallback())
            c.configure(buildFormat(lowLatency), surface, null, 0)
            c.start()
            return c
        }
        return try {
            val c = try {
                configureCodec(lowLatency = true)
            } catch (e: Exception) {
                Log.w(TAG, "low-latency configure failed (${e.message}); retrying without")
                // Codec.create may have leaked the half-initialised
                // instance; build a fresh one for the fallback.
                configureCodec(lowLatency = false)
            }
            codec = c
            // Don't reset `sawKeyframe` here — `stop()` already does, and
            // resetting again would clobber the flag that `feed()` may
            // have set while buffering a pre-codec IDR into
            // `pendingFrames`. Without this guard the just-buffered IDR
            // would get drained (good), then the very next P-frame
            // pushed via `feed()` would see `sawKeyframe = false` and
            // get rejected, and the decoder would idle until the next
            // IDR (one full GOP) — exactly the bug we're trying to fix.
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
        renderHandler?.removeCallbacksAndMessages(null)
        renderHandler = null
        renderThread?.quitSafely()
        renderThread = null
        try {
            c.stop()
        } catch (_: Exception) { }
        try {
            c.release()
        } catch (_: Exception) { }
    }

    fun feed(frame: VideoFrame) {
        // Two reasons codec might be null here:
        //  1) `start()` hasn't run yet — TextureView/SurfaceTexture isn't
        //     ready, so we're in the brief window between WS receiving
        //     `streamStarted` and the Compose tree finishing layout. The
        //     first IDR almost always lands here. We MUST hold onto it,
        //     otherwise the decoder spins waiting for the next IDR (one
        //     full GOP — up to 2 s — which manifests as a long black
        //     screen on every reconnect).
        //  2) `stop()` ran. Drop the frame in that case.
        // The lock and the `pendingFrames` ring serve both paths uniformly:
        // queue with bounded depth, and let `start()` drain whatever's
        // already there once the codec comes up.
        lock.withLock {
            if (!sawKeyframe) {
                if (!frame.isKeyframe) return
                sawKeyframe = true
            }
            val c = codec
            if (c == null) {
                pendingFrames.addLast(frame)
                while (pendingFrames.size > 32) pendingFrames.pollFirst()
                return
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
            // Anchor the clock origin against the audio path's published
            // reference if available — that's the master clock for
            // human-perceptible sync. If video decoded faster than audio
            // initialized, fall back to a standalone clock so the frame
            // renders *something* instead of stalling, then re-anchor as
            // soon as audio publishes. The one-time discontinuity at
            // re-anchor (a couple frames render slightly fast or slow as
            // the new origin shifts the schedule) is the price for
            // staying in sync once we have data; without it, late audio
            // arrival permanently locks video into a fallback clock that
            // doesn't match the actual audio playback wall time.
            val audioRef = avSync?.audioReference()
            if (clockOriginNs == 0L) {
                clockOriginNs = if (audioRef != null) {
                    anchoredOnAudio = true
                    lastAnchoredAudioWallNs = audioRef[0]
                    // audioRef[0] = wall ns when audio at audioRef[1]
                    // (microseconds, server clock) is audible. Setting
                    // origin so videoPts == audioRef[1] renders at the
                    // same wall ns aligns the two timelines exactly.
                    audioRef[0] - audioRef[1] * 1000L
                } else {
                    System.nanoTime() - info.presentationTimeUs * 1000L +
                        standaloneStartupLatencyNs
                }
                Log.i(
                    TAG,
                    "first-frame anchor: audio=${audioRef != null}" +
                        " video_pts=${info.presentationTimeUs}us" +
                        " now=${System.nanoTime()}" +
                        " clockOrigin=$clockOriginNs" +
                        if (audioRef != null) {
                            " audio_wall=${audioRef[0]} audio_pts=${audioRef[1]}us"
                        } else "",
                )
            } else if (audioRef != null) {
                // Audio reference may have shifted — either OpusPlayer
                // upgraded from seed to a `getTimestamp`-derived
                // refined value (large shift, hundreds of ms) or it's
                // tracking AudioTrack drift (small shift, ms-scale).
                // Re-anchor only when the new origin differs from the
                // current clock by more than 30 ms — below that the
                // visible jump in playback timing is worse than the
                // sync error it would correct. 30 ms ≈ one frame at
                // 30 fps, so any drift bigger than this is perceptible
                // and worth fixing.
                val newOrigin = audioRef[0] - audioRef[1] * 1000L
                val driftNs = kotlin.math.abs(newOrigin - clockOriginNs)
                if (driftNs > 30_000_000L) {
                    val oldOrigin = clockOriginNs
                    clockOriginNs = newOrigin
                    anchoredOnAudio = true
                    lastAnchoredAudioWallNs = audioRef[0]
                    Log.i(
                        TAG,
                        "re-anchor on audio: video_pts=${info.presentationTimeUs}us" +
                            " old_origin=$oldOrigin new_origin=$clockOriginNs" +
                            " shift_ms=${(clockOriginNs - oldOrigin) / 1_000_000}" +
                            " audio_wall=${audioRef[0]} audio_pts=${audioRef[1]}us",
                    )
                }
            }
            try {
                // Some hardware codecs (e.g. HiSi on PLR-AL30) ignore
                // the `releaseOutputBuffer(idx, renderTimeNs)` schedule
                // and just render ASAP — verified via
                // `setOnFrameRenderedListener` to fire ~400 ms before
                // our scheduled time on every frame. Manual delay via
                // a background handler is the only reliable way to
                // hold video back to match audio. We call the
                // immediate-render overload `(idx, true)` from the
                // delayed callback so the codec doesn't get a chance
                // to second-guess us.
                val renderTimeNs = clockOriginNs + info.presentationTimeUs * 1000L
                val delayNs = renderTimeNs - System.nanoTime()
                val delayMs = (delayNs / 1_000_000L).coerceIn(0L, maxRenderDelayMs)
                if (delayMs <= 0L) {
                    codec.releaseOutputBuffer(index, /* render = */ true)
                } else {
                    val handler = renderHandler
                    if (handler == null) {
                        codec.releaseOutputBuffer(index, /* render = */ true)
                    } else {
                        handler.postDelayed({
                            try {
                                codec.releaseOutputBuffer(index, /* render = */ true)
                            } catch (e: Exception) {
                                Log.w(TAG, "delayed releaseOutputBuffer", e)
                            }
                        }, delayMs)
                    }
                }
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
