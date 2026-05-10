package com.remotecontrol.app.video

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTimestamp
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

    /** Optional A/V sync rendezvous. We publish the wall-clock + source-PTS
     *  of the first audible audio sample here so the video player can
     *  anchor its render-clock to ours. */
    private var avSync: AvSyncClock? = null
    /** Set true once we publish the seed estimate on the first decoded
     *  audio sample. */
    private var avSyncSeeded: Boolean = false
    /** Last `frame0_wall` we published via `getTimestamp` refinement.
     *  Compared on subsequent `tryRefineAudioRef` calls so we don't
     *  publish the same value over and over (cheap dedup). The video
     *  player applies its own hysteresis on top — it only re-anchors
     *  when the published value moves more than ~30 ms. */
    private var lastRefinedFrame0WallNs: Long = 0L
    /** Source PTS (microseconds, server clock) of the very first audio
     *  sample we ever decoded. Captured on the first
     *  [onOutputBufferAvailable] with a non-empty payload and used for
     *  later `getTimestamp` refinement: AudioTrack's `framePosition` is
     *  the count of frames played since this first sample, so
     *  `frame_0_wall = ts.nanoTime - framePosition * 1e9 / sampleRate`
     *  and `(frame_0_wall, firstFramePtsUs)` is the exact reference
     *  point we want to publish. */
    private var firstFramePtsUs: Long = -1L
    /** Captured at `start()` from `info.sampleRate`, needed for the
     *  framePosition→wall-time math in the refinement path. */
    private var sampleRate: Int = 48000

    /** AudioTrack pre-roll latency estimate. We publish a *seed* value
     *  using `now + this estimate` on the very first decoded sample so
     *  the video player has something to anchor against early; we then
     *  refine the published value via `AudioTrack.getTimestamp()` on
     *  subsequent samples once the AudioTrack settles (usually within
     *  100-200 ms of bring-up).
     *
     *  The seed is intentionally on the *high* side of the device-typical
     *  range (AudioTrack buffer pre-fill + audio HAL + DAC together
     *  routinely hit 150-250 ms on stock Android phones, occasionally
     *  more on Bluetooth speakers). Overshooting means video lags audio
     *  by tens of ms initially, which is much less perceptible than
     *  undershooting and having video race ahead. The refinement step
     *  pulls it back to the actual play position once `getTimestamp` is
     *  reliable. */
    private val audioTrackPrerollNs: Long = 250_000_000L

    @Volatile var lastError: String? = null
        private set

    fun start(info: AudioStreamInfo, avSync: AvSyncClock? = null): Result<Unit> {
        stop()
        this.avSync = avSync
        this.avSyncSeeded = false
        this.lastRefinedFrame0WallNs = 0L
        this.firstFramePtsUs = -1L
        this.sampleRate = info.sampleRate
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
                // 2× minBuf: small enough that AudioTrack pre-roll stays
                // bounded (~50-300 ms depending on what the OEM chose
                // for `minBuf`), large enough to absorb a normal opus
                // packet arrival jitter. Earlier 8× buffer caused
                // ~2.5 s of pre-roll on at least one OEM device because
                // its `getMinBufferSize` returns a very pessimistic
                // value, which is fine for offline music but ruined
                // A/V sync for live streaming — video would render
                // promptly while audio sat in the buffer queue waiting
                // for its turn. Tradeoff: brief network jitters now
                // cause an audible glitch instead of a silent buffer
                // ride-through.
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
        // Mirror the H264Player pattern: queue frames into `pendingFrames`
        // even when the codec hasn't been built yet so audio that arrives
        // during MediaCodec/AudioTrack init isn't silently dropped. If we
        // dropped pre-codec audio (the original code just `?: return`'d
        // out), video would buffer the first ~500 ms of frames into its
        // own pendingFrames and replay from t=0, while audio threw those
        // same 500 ms away and started from t=500 — visible to the user
        // as the video sprinting half a second ahead of the audio. Now
        // both queues drain in lock-step on `start()`.
        lock.withLock {
            val c = codec
            if (c == null) {
                pendingFrames.addLast(frame)
                while (pendingFrames.size > 64) pendingFrames.pollFirst()
                return
            }
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
                    // Publish A/V sync reference on the first decoded
                    // sample with a non-empty payload. We do this *before*
                    // the (potentially blocking) `track.write` so the
                    // video player can pick up an estimate as early as
                    // possible — the offset is only `now + preroll` rather
                    // than the precise audible time, but it's stable enough
                    // for visual sync and lets H264Player anchor its clock
                    // origin instead of falling back to standalone mode.
                    if (!avSyncSeeded) {
                        avSyncSeeded = true
                        firstFramePtsUs = info.presentationTimeUs
                        val now = System.nanoTime()
                        val publishedAt = now + audioTrackPrerollNs
                        avSync?.publishAudioStart(publishedAt, firstFramePtsUs)
                        Log.i(
                            TAG,
                            "publishAudioStart seed: audio_pts=${firstFramePtsUs}us" +
                                " now=$now published_wall=$publishedAt" +
                                " preroll_ms=${audioTrackPrerollNs / 1_000_000}" +
                                " hasSync=${avSync != null}",
                        )
                    }
                    outBuf.position(info.offset)
                    outBuf.limit(info.offset + info.size)
                    track.write(outBuf, info.size, AudioTrack.WRITE_BLOCKING)
                    // Once the AudioTrack has been writing for a bit it
                    // will start to give us a real `getTimestamp`. Use it
                    // to compute the exact wall time at which frame 0 was
                    // played and replace the seed. This is the only way
                    // to get within ~1ms of the actual speaker output —
                    // any fixed pre-roll estimate has to deal with HAL
                    // latency that varies by 100s of ms across devices.
                    if (avSyncSeeded && firstFramePtsUs >= 0) {
                        tryRefineAudioRef(track)
                    }
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

    /**
     * Attempt to upgrade the seeded `(now + preroll)` AvSync publish to
     * a precise value computed from `AudioTrack.getTimestamp()`. The
     * timestamp gives us a `(framePosition, nanoTime)` pair where
     * `nanoTime` is the wall clock at which the sample at
     * `framePosition` was/will be heard. Since we always start writing
     * at sample 0 (track is fresh, never seeked), the wall time of
     * sample 0 = `nanoTime - framePosition * 1e9 / sampleRate`. Pair
     * that with `firstFramePtsUs` and we have the exact reference the
     * video player needs.
     *
     * Bails until the timestamp returns valid data with a positive
     * framePosition, which on Android typically takes 5-20 onOutput
     * callbacks (≈100-400 ms of audio) to settle. Once it succeeds and
     * we publish, [AvSyncClock.publishAudioStart] locks the value so
     * subsequent calls here are no-ops.
     */
    private fun tryRefineAudioRef(track: AudioTrack) {
        val ts = AudioTimestamp()
        val ok = try {
            track.getTimestamp(ts)
        } catch (e: Exception) {
            false
        }
        if (!ok || ts.framePosition <= 0L) return
        // Add the audio HAL's reported output latency on top of
        // `getTimestamp.nanoTime`. The timestamp tells us when a frame
        // crosses into the audio sink; HAL output latency is the gap
        // between that and the speaker actually producing sound (DAC,
        // amplifier, driver path). On many phones this is 50-150 ms
        // and was the missing piece causing video to perceptibly lead
        // audio even after our `getTimestamp` refinement.
        // Don't add `AudioTrack.getLatency()` — on at least one OEM
        // device (HiSi/PLR-AL30) it returns the entire buffer-queue
        // duration (2300 ms) rather than just speaker-output lag,
        // which double-counts what `getTimestamp.framePosition`
        // already accounts for. Just trust `getTimestamp` directly:
        // frame N has been (or will be) heard by ts.nanoTime, so
        // frame 0 was/will be heard at ts.nanoTime - N/sr.
        val halLatencyMs = 0L
        val frame0WallNs =
            ts.nanoTime - (ts.framePosition * 1_000_000_000L / sampleRate)
        // Hysteresis: if we already published a refined value within
        // 5 ms of this new computation, don't bother re-publishing —
        // the video player would otherwise re-anchor on every audio
        // frame, which adds churn for no perceptible benefit. 5 ms is
        // smaller than one video frame at 30 fps so any drift larger
        // than this is worth pushing through.
        if (lastRefinedFrame0WallNs != 0L &&
            kotlin.math.abs(frame0WallNs - lastRefinedFrame0WallNs) < 5_000_000L
        ) {
            return
        }
        avSync?.publishAudioStart(frame0WallNs, firstFramePtsUs)
        val isFirstRefine = lastRefinedFrame0WallNs == 0L
        lastRefinedFrame0WallNs = frame0WallNs
        if (isFirstRefine) {
            Log.i(
                TAG,
                "publishAudioStart refined:" +
                    " framePos=${ts.framePosition}" +
                    " ts.nanoTime=${ts.nanoTime}" +
                    " halLatencyMs=$halLatencyMs" +
                    " frame0_wall=$frame0WallNs" +
                    " audio_pts=${firstFramePtsUs}us",
            )
        }
    }
}
