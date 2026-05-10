package com.remotecontrol.app.video

/**
 * Shared A/V sync reference clock.
 *
 * The phone has two independent decoding pipelines — H264Player drives
 * video frames into a SurfaceView via `releaseOutputBuffer(idx, renderNs)`,
 * OpusPlayer feeds PCM into AudioTrack which clocks playback off the
 * device's sample-rate. They start asynchronously (different MediaCodec
 * init times, AudioTrack pre-buffer latency, etc.), so without an explicit
 * coordination point they drift apart on the *first frame*: whichever
 * pipeline becomes audible/visible first wins, and the other looks
 * "ahead" or "behind" by however many milliseconds its bring-up cost.
 *
 * The fix is to treat audio as the master clock — the human ear is far
 * more sensitive to audio glitches than to a half-second of black-screen
 * lead-in, and AudioTrack hands us a precise wall-clock timestamp for
 * any sample position via `getTimestamp`/`getPlaybackHeadPosition`. Once
 * the audio path knows when its first PCM sample is *audible* (not just
 * written), it publishes that wall-clock + the corresponding source PTS
 * here. The video path then computes its `clockOriginNs` against that
 * reference instead of `System.nanoTime() - first_pts`, so video PTS X
 * lands at the same wall clock as audio PTS X.
 *
 * If video happens to be ready before audio, [audioReference] returns
 * null and the caller falls back to a sensible default.
 *
 * Lifetime: one instance per logical stream session — created when a
 * `ConnectionClient` opens, dropped when the stream ends so leftover
 * timestamps don't leak into the next session.
 */
class AvSyncClock {
    @Volatile private var publishedWallNs: Long = -1L
    @Volatile private var publishedPtsUs: Long = -1L

    /**
     * Called by OpusPlayer to publish "audio sample at `ptsUs` (server
     * clock, microseconds) is audible at wall clock `wallNs`
     * (`System.nanoTime()`)".
     *
     * Always overwrites the previously published value. OpusPlayer
     * publishes a coarse seed on the first decoded sample (so the video
     * player has *something* to anchor against early), then keeps
     * refining via `AudioTrack.getTimestamp()` as the audio path
     * stabilises and as the audio clock drifts during playback (e.g.
     * when underruns reset framePosition). H264Player decides
     * separately whether the change is large enough to warrant
     * re-anchoring its render clock.
     */
    fun publishAudioStart(wallNs: Long, ptsUs: Long) {
        publishedWallNs = wallNs
        publishedPtsUs = ptsUs
    }

    /**
     * Returns `[wallNs, ptsUs]` if audio has started, else null. Caller
     * uses these to set its own playback origin so wall-clock for any
     * source PTS matches audio's.
     */
    fun audioReference(): LongArray? {
        if (publishedPtsUs < 0) return null
        return longArrayOf(publishedWallNs, publishedPtsUs)
    }

    /**
     * Forget any previously published audio start. Call this between
     * stream sessions so the next session's first audio sample wins
     * rather than getting suppressed by a stale value from the previous
     * connection.
     */
    fun reset() {
        publishedWallNs = -1L
        publishedPtsUs = -1L
    }
}
