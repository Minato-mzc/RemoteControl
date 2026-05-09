package com.remotecontrol.app.model

import android.net.Uri

/**
 * Parsed result of scanning a QR code from the PC server.
 *
 * Two URL schemes are recognized:
 *
 *  1. **`rc://host:port/?v=N&c=CODE&k=KEY_B64URL`** — direct LAN
 *     connection. The phone opens `ws://host:port/ws` and runs the M1
 *     pairing handshake.
 *
 *  2. **`rcrelay://relay.example.com:port/?host=HOST_ID&v=N&c=CODE&k=KEY_B64URL`**
 *     — cross-network connection via a user-deployed relay. The phone
 *     opens `wss://relay.example.com/v1/client?host=HOST_ID` and the
 *     same M1 handshake runs *through the tunnel*. From the phone's
 *     point of view the protocol is identical to the LAN path; the
 *     relay just brokers reachability.
 *
 * The two cases collapse into the same data class: only [wsUrl] differs.
 * Everything downstream (HMAC challenge, trusted reconnect, stream
 * lifecycle) is transport-agnostic.
 */
data class QrPayload(
    val host: String,
    val port: Int,
    val code: String,
    val keyB64Url: String,
    val version: Int,
    /** When non-null, this is a relay payload — `host` is the relay's
     *  domain and we hit `/v1/client?host=...` instead of `/ws`. */
    val relayHostId: String? = null,
    /** When true, prefer `wss://` over `ws://`. Set automatically for
     *  relay scheme (TLS termination expected on the relay side); LAN
     *  payloads default to plain `ws://`. */
    val secure: Boolean = false,
) {
    val wsUrl: String
        get() = when {
            relayHostId != null -> {
                val scheme = if (secure) "wss" else "ws"
                "$scheme://$host:$port/v1/client?host=$relayHostId"
            }
            else -> {
                val scheme = if (secure) "wss" else "ws"
                "$scheme://$host:$port/ws"
            }
        }

    companion object {
        fun parse(raw: String): QrPayload? {
            return runCatching {
                val uri = Uri.parse(raw.trim())
                val scheme = uri.scheme?.lowercase() ?: return@runCatching null
                when (scheme) {
                    "rc" -> parseLan(uri)
                    "rcrelay" -> parseRelay(uri)
                    else -> null
                }
            }.getOrNull()?.let { it }
        }

        private fun parseLan(uri: Uri): QrPayload? {
            val host = uri.host ?: return null
            val port = uri.port.takeIf { it > 0 } ?: return null
            val version = uri.getQueryParameter("v")?.toIntOrNull() ?: return null
            val code = uri.getQueryParameter("c")?.takeIf { it.isNotBlank() } ?: return null
            val key = uri.getQueryParameter("k")?.takeIf { it.isNotBlank() } ?: return null
            return QrPayload(host, port, code, key, version)
        }

        private fun parseRelay(uri: Uri): QrPayload? {
            val host = uri.host ?: return null
            // Relay default port is 443 (TLS via caddy/nginx). The PC's
            // QR may include an explicit port for non-default deploys.
            val port = uri.port.takeIf { it > 0 } ?: 443
            val version = uri.getQueryParameter("v")?.toIntOrNull() ?: return null
            val code = uri.getQueryParameter("c")?.takeIf { it.isNotBlank() } ?: return null
            val key = uri.getQueryParameter("k")?.takeIf { it.isNotBlank() } ?: return null
            val hostId = uri.getQueryParameter("host")?.takeIf { it.isNotBlank() }
                ?: return null
            return QrPayload(
                host = host,
                port = port,
                code = code,
                keyB64Url = key,
                version = version,
                relayHostId = hostId,
                // Relays sit behind TLS by deployment convention.
                secure = true,
            )
        }
    }
}
