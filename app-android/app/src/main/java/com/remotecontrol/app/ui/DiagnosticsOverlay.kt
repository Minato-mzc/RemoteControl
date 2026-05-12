package com.remotecontrol.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Floating diagnostic panel pinned to the top-start of the screen.
 * Shows live link health for the connection driving the video stream
 * — what the user sees on the phone vs what's actually on the wire.
 *
 * Surfaced via the keyboard panel's "📊 诊断" button; the toggle is
 * a no-op when this composable isn't rendered, so the user can dismiss
 * either with that button or the X in the panel itself.
 *
 * Color coding:
 *   * green for healthy values,
 *   * amber for marginal (RTT 100–250 ms, fps 15–25, frame age 100–500 ms),
 *   * red for bad (RTT >250 ms, fps <15, frame age >500 ms).
 *
 * Designed to read on top of busy video frames — semi-transparent
 * black background + monospace numbers + accent-coloured values.
 */
@Composable
fun DiagnosticsOverlay(
    metrics: LinkMetrics,
    onDismiss: () -> Unit,
) {
    Box(
        modifier = Modifier
            .fillMaxSize()
            // Don't intercept gestures elsewhere on the screen — only
            // the panel itself needs to be tappable for the close
            // button. The outer Box is just a layout container.
            .padding(top = 56.dp, start = 12.dp),
        contentAlignment = Alignment.TopStart,
    ) {
        Column(
            modifier = Modifier
                .background(Color(0xCC101418), RoundedCornerShape(10.dp))
                .padding(horizontal = 12.dp, vertical = 8.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "📊 链路诊断",
                    color = Color.White,
                    fontSize = 12.sp,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(end = 8.dp),
                )
                Spacer(Modifier.width(8.dp))
                IconButton(
                    onClick = onDismiss,
                    modifier = Modifier.width(28.dp).height(28.dp),
                ) {
                    Icon(
                        Icons.Default.Close,
                        contentDescription = "关闭诊断",
                        tint = Color(0xFFB0B0B0),
                    )
                }
            }
            Spacer(Modifier.height(6.dp))
            MetricRow("RTT", formatRtt(metrics.rttMs), rttColor(metrics.rttMs))
            MetricRow("FPS", "%.1f".format(metrics.fps), fpsColor(metrics.fps))
            MetricRow("码率", "%.2f Mbps".format(metrics.mbps), null)
            MetricRow(
                "帧年龄",
                formatAge(metrics.lastFrameAgeMs),
                ageColor(metrics.lastFrameAgeMs),
            )
        }
    }
}

@Composable
private fun MetricRow(label: String, value: String, valueColor: Color?) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            label,
            color = Color(0xFF9CA3AF),
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace,
            modifier = Modifier.width(56.dp),
        )
        Text(
            value,
            color = valueColor ?: Color.White,
            fontSize = 12.sp,
            fontFamily = FontFamily.Monospace,
            fontWeight = FontWeight.Medium,
        )
    }
}

// Color helpers below are deliberately simple — three buckets each.

private fun formatRtt(rtt: Long?): String = when {
    rtt == null -> "—"
    rtt < 1000 -> "${rtt} ms"
    else -> "%.1f s".format(rtt / 1000.0)
}

private fun rttColor(rtt: Long?): Color? = when {
    rtt == null -> null
    rtt < 100 -> GREEN
    rtt < 250 -> AMBER
    else -> RED
}

private fun fpsColor(fps: Float): Color? = when {
    fps >= 25f -> GREEN
    fps >= 15f -> AMBER
    fps > 0f -> RED
    else -> null
}

private fun formatAge(age: Long?): String = when {
    age == null -> "—"
    age < 1000 -> "${age} ms"
    else -> "%.1f s".format(age / 1000.0)
}

private fun ageColor(age: Long?): Color? = when {
    age == null -> null
    age < 100 -> GREEN
    age < 500 -> AMBER
    else -> RED
}

private val GREEN = Color(0xFF4ADE80)
private val AMBER = Color(0xFFFBBF24)
private val RED = Color(0xFFF87171)
