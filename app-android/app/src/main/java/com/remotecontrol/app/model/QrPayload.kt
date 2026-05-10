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
            val version = uri.getQueryParameter("v")?.toIntOrNull() ?: return null
            val code = uri.getQueryParameter("c")?.takeIf { it.isNotBlank() } ?: return null
            val key = uri.getQueryParameter("k")?.takeIf { it.isNotBlank() } ?: return null
            val hostId = uri.getQueryParameter("host")?.takeIf { it.isNotBlank() }
                ?: return null
            // `tls` is set by the server based on its configured `base_url`:
            // `https://...` → tls=1 (production caddy/Let's Encrypt deploy),
            // `http://...`  → tls=0 (LAN-only smoke test of the relay
            // protocol stack against a plain-HTTP relay binary, or a
            // Mainland VPS using a non-standard port without ICP filing).
            // Default to true (TLS) when the param is absent so old QRs
            // generated before this field existed still favor secure
            // transport rather than silently downgrading.
            val tls = uri.getQueryParameter("tls")?.let { it != "0" } ?: true
            // Port fallback follows the scheme: tls=1 → 443 (https/wss),
            // tls=0 → 80 (http/ws). Hardcoding 443 here was wrong for
            // plain-HTTP deploys: `rcrelay://1.2.3.4/?...&tls=0` (no
            // explicit port) used to dial 443 and time out. The PC side
            // also injects an explicit port into the QR payload now, but
            // we still want the parser to do the right thing on its own
            // for backwards-compatible QRs and edge cases.
            val port = uri.port.takeIf { it > 0 } ?: if (tls) 443 else 80
            return QrPayload(
                host = host,
                port = port,
                code = code,
                keyB64Url = key,
                version = version,
                relayHostId = hostId,
                secure = tls,
            )
        }
    }
}
