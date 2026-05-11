package com.remotecontrol.app.model

import android.net.Uri

/**
 * Parsed result of scanning a QR code from the PC server.
 *
 * Three URL shapes are accepted (the parser normalises them all into
 * the same `QrPayload`):
 *
 *  1. **Pure LAN**: `rc://host:port/?v=N&c=CODE&k=KEY`
 *     The phone opens `ws://host:port/ws` and runs M1 pairing.
 *
 *  2. **Pure relay (legacy)**: `rcrelay://relay.host:port/?host=HOST_ID&v=N&c=CODE&k=KEY&tls=0|1`
 *     The phone opens `ws://relay.host:port/v1/client?host=HOST_ID`.
 *
 *  3. **Combined (preferred)**: `rc://lan_host:lan_port/?v=N&c=CODE&k=KEY&relay=<authority>;<host_id>;<tls>`
 *     Encodes both transport candidates in a single QR. The phone tries
 *     LAN first; if it can't reach `lan_host:lan_port` within a short
 *     window the connection logic falls back to the relay candidate.
 *
 * Anything downstream of `QrPayload` (HMAC challenge, trusted reconnect,
 * stream lifecycle) is transport-agnostic — picking which WS URL to dial
 * is the only difference.
 */
data class QrPayload(
    val host: String,
    val port: Int,
    val code: String,
    val keyB64Url: String,
    val version: Int,
    /** When non-null, this is the relay's `host_id` — `host:port` in
     *  the data class then refers to the relay endpoint. We hit
     *  `/v1/client?host=...` on this URL. */
    val relayHostId: String? = null,
    /** When true, prefer `wss://` over `ws://` for the *primary* dial.
     *  Set automatically when the relay configured TLS termination. */
    val secure: Boolean = false,
    /** Optional second-chance dial: scanned from the combined QR's
     *  `&relay=` query param. If [host] is unreachable (same-network
     *  scan from across the internet) we retry using this. Always
     *  `null` for legacy single-mode QRs. */
    val fallback: QrPayload? = null,
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
            // Combined-QR extension: optional `relay=<authority>;<host_id>;<tls>`
            // query param. Older app builds and pure-LAN QRs simply
            // skip this; newer builds use it as a fallback dial when
            // the LAN endpoint isn't reachable.
            val relayParam = uri.getQueryParameter("relay")
            val fallback = relayParam?.let { parseRelayTuple(it, version, code, key) }
            return QrPayload(host, port, code, key, version, fallback = fallback)
        }

        /**
         * Decode the `relay=` query value emitted by the PC's
         * [build_combined_payload]. Format:
         * `<authority>;<host_id>;<tls_flag>`, e.g.
         * `150.158.45.221:8443;abc-uuid;0`. Returns null for any
         * shape that doesn't parse — the caller treats it as "no
         * fallback, LAN only".
         */
        private fun parseRelayTuple(
            raw: String,
            version: Int,
            code: String,
            key: String,
        ): QrPayload? {
            val parts = raw.split(";")
            if (parts.size != 3) return null
            val authority = parts[0].takeIf { it.isNotBlank() } ?: return null
            val hostId = parts[1].takeIf { it.isNotBlank() } ?: return null
            val tls = parts[2] != "0"
            val (host, port) = parseAuthority(authority, tls) ?: return null
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

        private fun parseAuthority(authority: String, tls: Boolean): Pair<String, Int>? {
            val idx = authority.lastIndexOf(':')
            if (idx <= 0) {
                // No explicit port — use scheme default.
                return Pair(authority, if (tls) 443 else 80)
            }
            val host = authority.substring(0, idx)
            val port = authority.substring(idx + 1).toIntOrNull() ?: return null
            return Pair(host, port)
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
