package com.remotecontrol.app.model

import android.net.Uri

/**
 * Parsed from a `rc://host:port/?v=1&c=CODE&k=KEY_B64URL` payload shown by the PC server.
 */
data class QrPayload(
    val host: String,
    val port: Int,
    val code: String,
    val keyB64Url: String,
    val version: Int,
) {
    val wsUrl: String get() = "ws://$host:$port/ws"

    companion object {
        fun parse(raw: String): QrPayload? {
            return runCatching {
                val uri = Uri.parse(raw.trim())
                if (!uri.scheme.equals("rc", ignoreCase = true)) return@runCatching null
                val host = uri.host ?: return@runCatching null
                val port = uri.port.takeIf { it > 0 } ?: return@runCatching null
                val version = uri.getQueryParameter("v")?.toIntOrNull() ?: return@runCatching null
                val code = uri.getQueryParameter("c")?.takeIf { it.isNotBlank() }
                    ?: return@runCatching null
                val key = uri.getQueryParameter("k")?.takeIf { it.isNotBlank() }
                    ?: return@runCatching null
                QrPayload(host, port, code, key, version)
            }.getOrNull()?.let { it }
        }
    }
}
