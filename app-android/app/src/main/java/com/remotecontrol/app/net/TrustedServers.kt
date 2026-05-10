package com.remotecontrol.app.net

import android.content.Context
import androidx.core.content.edit
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json

/**
 * A previously-paired server we can reconnect to without scanning a QR.
 *
 * After the first successful [Hello] handshake, the server hands back a
 * `device_id` + 256-bit `trust_token` in [Welcome]. We persist them here,
 * tagged with the `wsUrl` we connected to last time, so the next app
 * launch can skip straight to a [TrustedHello] reconnect.
 */
@Serializable
data class TrustedServer(
    val deviceId: String,
    val token: String,
    /** Last `ws://host:port` we successfully connected on. The server's
     *  IP can change between Wi-Fi sessions — if this URL stops working
     *  the UI falls back to QR scan. */
    val wsUrl: String,
    /** Display name from `Welcome.server.name` ("ADMIN", etc.). */
    val serverName: String,
    /** Wall-clock millis of last successful connection — for sorting and
     *  for showing "上次连接 X 分钟前" later. */
    val lastConnectedMs: Long,
)

/**
 * SharedPreferences-backed store of trusted servers. Persistent across
 * app restarts, app updates, and device reboots; cleared on app data
 * wipe. Synchronously read/written — the dataset is tiny (one entry per
 * paired PC) so no async wrapper.
 */
class TrustedServerStore(context: Context) {

    private val prefs = context.applicationContext.getSharedPreferences(
        PREFS_NAME, Context.MODE_PRIVATE,
    )
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val listSerializer = ListSerializer(TrustedServer.serializer())

    fun list(): List<TrustedServer> {
        val raw = prefs.getString(KEY_LIST, null) ?: return emptyList()
        val parsed = runCatching { json.decodeFromString(listSerializer, raw) }
            .getOrDefault(emptyList())
        // Coalesce duplicates left over from earlier app versions that
        // didn't dedup by `serverName` on insert. Without this, users who
        // re-paired the same PC several times under the old code would
        // see the same machine name listed multiple times in the
        // "重试" UI even after we fix `upsert`. Keep the most recently
        // connected entry per server name; persist the cleaned list so
        // the file shrinks on first load and the next `list()` call is
        // O(1) again.
        val deduped = dedupeByServerName(parsed)
        if (deduped.size != parsed.size) {
            save(deduped)
        }
        return deduped
    }

    /**
     * Insert-or-update keyed by *both* [TrustedServer.deviceId] AND
     * [TrustedServer.serverName]. The deviceId dedup covers the normal
     * trusted-reconnect path: same phone reconnects to same PC, server
     * recognises the device, we update timestamp + wsUrl in place.
     *
     * The serverName dedup is what makes "scan the QR again on the same
     * PC" do the right thing: every fresh `Hello` (QR scan) on the PC
     * server calls `trusted_devices.mint()`, which always generates a
     * brand-new UUID. Without name-based dedup, the phone sees that as
     * a *different* device and adds a second "重试 ADMIN" entry; scan
     * a few more times and the list balloons. Treating same name as
     * same machine collapses these correctly. Tradeoff: if the user
     * pairs two PCs that happen to share a default hostname like
     * "ADMIN" or "DESKTOP-XXX", the second pair replaces the first
     * here — they need to give the PCs distinct names. Acceptable for
     * the common case (one PC per user).
     */
    fun upsert(server: TrustedServer) {
        val current = list().filter {
            it.deviceId != server.deviceId && it.serverName != server.serverName
        }
        val merged = (listOf(server) + current).take(MAX_ENTRIES)
        save(merged)
    }

    private fun dedupeByServerName(items: List<TrustedServer>): List<TrustedServer> {
        // Items are stored most-recent-first so a forward iteration with
        // a "seen names" set keeps the freshest entry per name and
        // discards the rest.
        val seen = HashSet<String>()
        val out = ArrayList<TrustedServer>(items.size)
        for (s in items) {
            if (seen.add(s.serverName)) {
                out.add(s)
            }
        }
        return out
    }

    /** Drop a single entry — used when the server returns BadTrustToken
     *  (token rotated / revoked) so the saved entry stays in sync with
     *  the server's view. */
    fun forget(deviceId: String) {
        val remaining = list().filter { it.deviceId != deviceId }
        save(remaining)
    }

    /** Wipe every entry — for the eventual "Forget all paired PCs"
     *  settings button. */
    fun clearAll() {
        prefs.edit { remove(KEY_LIST) }
    }

    private fun save(items: List<TrustedServer>) {
        val raw = json.encodeToString(listSerializer, items)
        prefs.edit { putString(KEY_LIST, raw) }
    }

    companion object {
        private const val PREFS_NAME = "remotecontrol_trust"
        private const val KEY_LIST = "trusted_servers_v1"

        /** Practical upper bound — same user is unlikely to pair more
         *  than a handful of PCs. Keeps the list bounded if the file
         *  somehow gets churned. */
        private const val MAX_ENTRIES = 16
    }
}
