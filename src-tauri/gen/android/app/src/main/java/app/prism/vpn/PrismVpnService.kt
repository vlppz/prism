package app.prism.vpn

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.IpPrefix
import android.net.Network
import android.net.VpnService
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.net.InetAddress
import kotlin.concurrent.thread

/** The session core in Rust (src-tauri/src/android_core.rs). */
object PrismNative {
    init {
        // the VPN service and the tile can run without the app's window
        System.loadLibrary("prism_lib")
    }

    /** VpnService.Builder plan as JSON, or "!" + why it can't connect. */
    @JvmStatic external fun plan(dataDir: String): String
    @JvmStatic external fun run(fd: Int)
    @JvmStatic external fun failed(message: String)
    /** Blocks until the tunnel is down (at most ~10 s): not on the main thread. */
    @JvmStatic external fun stop()
    /** {state, name, since, rx, tx, error} */
    @JvmStatic external fun status(): String
    @JvmStatic external fun isRunning(): Boolean
    @JvmStatic external fun hasConfig(dataDir: String): Boolean
    @JvmStatic external fun networkChanged()
}

/**
 * Prism's VPN. It owns the session, so it runs the same whether the app, the
 * Quick Settings tile or Android (restarting it) started it, and it keeps
 * running as a foreground service with a notification, so the system leaves
 * it alone while the phone sleeps.
 */
class PrismVpnService : VpnService() {
    companion object {
        private const val TAG = "PrismVpn"
        const val ACTION_CONNECT = "app.prism.vpn.SERVICE_CONNECT"
        const val ACTION_DISCONNECT = "app.prism.vpn.SERVICE_DISCONNECT"
        private const val CHANNEL = "connection"
        private const val NOTE_ID = 1
        private const val PREFS = "prism"
        /** Set while a session should be up: after Android kills Prism, it comes back. */
        private const val WANT_ON = "wantOn"

        fun connect(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, PrismVpnService::class.java).setAction(ACTION_CONNECT))
        }

        /** Disconnects off the main thread (stopping waits for the engine). */
        fun disconnect(context: Context) {
            setWanted(context, false)
            thread(name = "prism-stop") { PrismNative.stop() }
        }

        fun setWanted(context: Context, on: Boolean) {
            context.getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean(WANT_ON, on).apply()
        }

        private fun wanted(context: Context) = context.getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(WANT_ON, false)
    }

    private val main = Handler(Looper.getMainLooper())
    private var connecting = false
    private var polling = false
    private var callback: ConnectivityManager.NetworkCallback? = null
    private var last: Triple<Long, Long, Long>? = null // time, rx, tx

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_DISCONNECT -> {
                disconnect(this)
                return START_NOT_STICKY
            }
            // the app, the tile, or the system's always-on VPN
            ACTION_CONNECT, SERVICE_INTERFACE -> start()
            // null: Android restarted us after killing the process
            else -> if (wanted(this)) start() else if (!PrismNative.isRunning()) stopSelf()
        }
        return START_STICKY
    }

    private fun start() {
        foreground(buildNotification(JSONObject().put("state", "connecting")))
        if (connecting) return
        connecting = true
        setWanted(this, true)
        startPolling()
        thread(name = "prism-connect") {
            try {
                val plan = PrismNative.plan(dataDir.absolutePath)
                if (!plan.startsWith("!")) {
                    PrismNative.run(establish(JSONObject(plan)))
                    main.post { startNetworkWatch() }
                }
            } catch (e: Exception) {
                Log.w(TAG, "connect failed", e)
                PrismNative.failed(e.message ?: e.toString())
            } finally {
                main.post { connecting = false }
            }
        }
    }

    private fun foreground(n: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTE_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTE_ID, n)
        }
    }

    // ---------- the interface ----------

    private fun establish(plan: JSONObject): Int {
        fun list(key: String) = plan.optJSONArray(key)?.let { a -> (0 until a.length()).map { a.getString(it) } } ?: emptyList()
        val b = Builder()
            .setSession(plan.optString("session", "Prism"))
            .setMtu(plan.optInt("mtu", 1280))
            // Prism itself stays outside: the WireGuard socket and its DNS
            // lookups use the real network.
            .addDisallowedApplication(packageName)
        for (a in list("addresses")) cidr(a).let { b.addAddress(it.first, it.second) }
        for (d in list("dns")) b.addDnsServer(d)
        for (r in list("routes")) {
            try {
                cidr(r).let { b.addRoute(it.first, it.second) }
            } catch (e: IllegalArgumentException) {
                Log.w(TAG, "route $r skipped: ${e.message}")
            }
        }
        // smart routing: Android 13+ can leave networks outside the VPN
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            for (r in list("excludes")) cidr(r).let { b.excludeRoute(IpPrefix(InetAddress.getByName(it.first), it.second)) }
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) b.setMetered(false)
        b.setConfigureIntent(openApp())
        val pfd = b.establish() ?: throw IllegalStateException("Android didn't allow the VPN. Open Prism and connect again")
        return pfd.detachFd()
    }

    private fun cidr(s: String): Pair<String, Int> {
        val i = s.indexOf('/')
        return if (i < 0) Pair(s, if (s.contains(':')) 128 else 32) else Pair(s.substring(0, i), s.substring(i + 1).toInt())
    }

    override fun onRevoke() {
        Log.i(TAG, "VPN revoked")
        disconnect(this)
    }

    override fun onDestroy() {
        polling = false
        main.removeCallbacksAndMessages(null)
        stopNetworkWatch()
        PrismTileService.refresh(this)
        super.onDestroy()
    }

    // ---------- status: notification, tile, and stopping when the session ends ----------

    private fun startPolling() {
        if (polling) return
        polling = true
        main.post(object : Runnable {
            override fun run() {
                if (!polling) return
                val s = JSONObject(PrismNative.status())
                if (s.optString("state") == "off" && !connecting) {
                    finish(s.optString("error"))
                    return
                }
                getSystemService(NotificationManager::class.java)?.notify(NOTE_ID, buildNotification(s))
                main.postDelayed(this, 1000)
            }
        })
        PrismTileService.refresh(this)
    }

    /** The session is over: drop the notification and the service. */
    private fun finish(error: String) {
        polling = false
        setWanted(this, false)
        stopNetworkWatch()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) stopForeground(STOP_FOREGROUND_REMOVE) else @Suppress("DEPRECATION") stopForeground(true)
        // started from the tile there's no window to say why it stopped
        if (error.isNotEmpty() && error != "null") {
            val n = NotificationCompat.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_tile)
                .setContentTitle("Prism couldn't connect")
                .setContentText(error.replaceFirstChar { it.uppercase() })
                .setStyle(NotificationCompat.BigTextStyle().bigText(error.replaceFirstChar { it.uppercase() }))
                .setContentIntent(openApp())
                .setAutoCancel(true)
                .build()
            getSystemService(NotificationManager::class.java)?.notify(NOTE_ID + 1, n)
        }
        stopSelf()
        PrismTileService.refresh(this)
    }

    private fun openApp(): PendingIntent =
        PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

    private fun buildNotification(s: JSONObject): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && nm?.getNotificationChannel(CHANNEL) == null) {
            nm?.createNotificationChannel(
                NotificationChannel(CHANNEL, "Connection", NotificationManager.IMPORTANCE_LOW).apply {
                    description = "Shows while Prism is connected"
                    setShowBadge(false)
                }
            )
        }
        val name = s.optString("name").ifEmpty { "Prism" }
        val state = s.optString("state")
        val title = when (state) {
            "connected" -> "Connected · $name"
            "unreachable" -> "No answer from $name"
            "stalled" -> "Reconnecting to $name"
            else -> "Connecting to $name"
        }
        val text = if (state == "connected") rates(s.optLong("rx"), s.optLong("tx")) else when (state) {
            "unreachable" -> "The server's IP may be blocked here. Still trying"
            "stalled" -> "Traffic was throttled. Switching port"
            else -> "Masking the handshake"
        }
        val disconnect = PendingIntent.getService(
            this, 1,
            Intent(this, PrismVpnService::class.java).setAction(ACTION_DISCONNECT),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val b = NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(title)
            .setContentText(text)
            .setContentIntent(openApp())
            .addAction(0, "Disconnect", disconnect)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setShowWhen(false)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
        val since = s.optLong("since", 0)
        if (state == "connected" && since > 0) {
            // the session clock, as in the app
            b.setShowWhen(true).setUsesChronometer(true).setWhen(since)
        }
        return b.build()
    }

    private fun rates(rx: Long, tx: Long): String {
        val now = SystemClock.elapsedRealtime()
        val prev = last
        last = Triple(now, rx, tx)
        if (prev == null || now <= prev.first) return "↓ —   ↑ —"
        val dt = (now - prev.first) / 1000.0
        return "↓ ${speed((rx - prev.second).coerceAtLeast(0) / dt)}   ↑ ${speed((tx - prev.third).coerceAtLeast(0) / dt)}"
    }

    private fun speed(bps: Double): String = when {
        bps < 1000 -> "${bps.toInt()} B/s"
        bps < 1e6 -> String.format(if (bps < 1e4) "%.1f KB/s" else "%.0f KB/s", bps / 1e3)
        else -> String.format(if (bps < 1e8) "%.1f MB/s" else "%.0f MB/s", bps / 1e6)
    }

    // ---------- network switches ----------

    /** The WireGuard socket is tied to one network; tell the engine when it changes. */
    private fun startNetworkWatch() {
        if (callback != null) return
        val cm = getSystemService(ConnectivityManager::class.java) ?: return
        val cb = object : ConnectivityManager.NetworkCallback() {
            private var current: Network? = null

            override fun onAvailable(network: Network) {
                val before = current
                current = network
                if (before != null && before != network) PrismNative.networkChanged()
            }
        }
        try {
            // the app is outside its own VPN, so this is the real network
            cm.registerDefaultNetworkCallback(cb)
            callback = cb
        } catch (e: RuntimeException) {
            Log.w(TAG, "network watch unavailable: ${e.message}")
        }
    }

    private fun stopNetworkWatch() {
        val cb = callback ?: return
        callback = null
        try {
            getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(cb)
        } catch (_: RuntimeException) {
        }
    }
}
