package app.prism.vpn

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.OnBackPressedCallback
import android.os.Build
import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import android.net.VpnService
import android.provider.OpenableColumns
import android.webkit.WebView
import androidx.appcompat.app.AppCompatActivity
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanIntentResult
import com.journeyapps.barcodescanner.ScanOptions
import kotlin.concurrent.thread

@InvokeArg
class UrlArgs {
    var url: String = ""
}

/** Rust's handle on Android (src-tauri/src/android.rs). */
@TauriPlugin
class VpnPlugin(private val activity: Activity) : Plugin(activity) {
    /** The tile asked to connect while Prism wasn't running. */
    @Volatile
    private var connectRequested = false

    /** A config opened with Prism ("Open with", "Share") waiting for the page. */
    @Volatile
    private var opened: Intent? = null

    override fun load(webView: WebView) {
        keep(activity.intent)
        // Back closes the page's open sheet; with none open Prism goes to the
        // background instead of closing (the VPN keeps running either way)
        (activity as? AppCompatActivity)?.onBackPressedDispatcher?.addCallback(object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                webView.evaluateJavascript("window.__prismBack ? window.__prismBack() : false") { handled ->
                    if (handled != "true") activity.moveTaskToBack(true)
                }
            }
        })
    }

    override fun onNewIntent(intent: Intent) {
        keep(intent)
    }

    private fun keep(intent: Intent?) {
        if (intent?.action == PrismTileService.ACTION_CONNECT) {
            connectRequested = true
            // handled once; a later recreation must not connect again
            intent.action = Intent.ACTION_MAIN
            return
        }
        if (intent?.action == Intent.ACTION_VIEW || intent?.action == Intent.ACTION_SEND) opened = intent
    }

    @Command
    fun prepare(invoke: Invoke) {
        // the connection notification needs this on Android 13+ (asked once)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED &&
            !prefs().getBoolean("askedNotifications", false)
        ) {
            prefs().edit().putBoolean("askedNotifications", true).apply()
            asking = true
            activity.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 7301)
        }
        val intent = VpnService.prepare(activity)
        if (intent == null) {
            invoke.resolve(JSObject().put("granted", true))
        } else {
            startActivityForResult(invoke, intent, "onPrepared")
        }
    }

    @ActivityCallback
    fun onPrepared(invoke: Invoke, result: ActivityResult) {
        invoke.resolve(JSObject().put("granted", result.resultCode == Activity.RESULT_OK))
    }

    /** True while a system permission dialog from [prepare] may be on screen. */
    private var asking = false

    private fun prefs() = activity.getSharedPreferences("prism", Activity.MODE_PRIVATE)

    @Command
    fun connect(invoke: Invoke) {
        try {
            PrismVpnService.connect(activity)
        } catch (e: Exception) {
            invoke.reject("couldn't start the VPN service: ${e.message}")
            return
        }
        invoke.resolve()
        askBatteryOnce()
    }

    /**
     * Samsung and others put "optimized" apps to sleep and cut their network
     * with the screen off; a VPN has to be exempt. Asked once, after the first
     * connect (not on top of another permission dialog).
     */
    private fun askBatteryOnce() {
        if (asking) {
            asking = false
            return
        }
        val pm = activity.getSystemService(PowerManager::class.java) ?: return
        if (pm.isIgnoringBatteryOptimizations(activity.packageName) || prefs().getBoolean("askedBattery", false)) return
        prefs().edit().putBoolean("askedBattery", true).apply()
        try {
            @Suppress("BatteryLife")
            activity.startActivity(
                Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${activity.packageName}"))
            )
        } catch (_: Exception) {
        }
    }

    /** Camera QR scanner (ZXing: no Google services needed). */
    @Command
    fun scanQr(invoke: Invoke) {
        val options = ScanOptions()
            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
            .setPrompt("Point the camera at a WireGuard QR code")
            .setBeepEnabled(false)
            .setOrientationLocked(false)
        startActivityForResult(invoke, ScanContract().createIntent(activity, options), "onScanned")
    }

    @ActivityCallback
    fun onScanned(invoke: Invoke, result: ActivityResult) {
        val scan = ScanIntentResult.parseActivityResult(result.resultCode, result.data)
        invoke.resolve(JSObject().put("text", scan.contents))
    }

    @Command
    fun takeConnect(invoke: Invoke) {
        val on = connectRequested
        connectRequested = false
        invoke.resolve(JSObject().put("connect", on))
    }

    @Command
    fun openUrl(invoke: Invoke) {
        val args = invoke.parseArgs(UrlArgs::class.java)
        try {
            activity.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(args.url)))
        } catch (_: Exception) {
        }
        invoke.resolve()
    }

    @Command
    fun clipboard(invoke: Invoke) {
        val cm = activity.getSystemService(ClipboardManager::class.java)
        val text = cm?.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(activity)?.toString()
        invoke.resolve(JSObject().put("text", text ?: ""))
    }

    /** The config a file manager or messenger handed us, read once. */
    @Command
    fun takeOpened(invoke: Invoke) {
        val intent = opened
        opened = null
        thread(name = "prism-open") {
            val out = JSObject()
            try {
                if (intent != null) {
                    val shared = intent.getStringExtra(Intent.EXTRA_TEXT)
                    @Suppress("DEPRECATION")
                    val uri = intent.data ?: intent.getParcelableExtra(Intent.EXTRA_STREAM) as? Uri
                    if (uri != null) {
                        out.put("name", displayName(uri))
                        val type = intent.type ?: activity.contentResolver.getType(uri) ?: ""
                        if (type.startsWith("image/")) {
                            // a QR code picture: Rust reads the code
                            out.put("image", android.util.Base64.encodeToString(readBytes(uri, 20_000_000), android.util.Base64.NO_WRAP))
                        } else {
                            out.put("text", read(uri))
                        }
                    } else if (shared != null) {
                        out.put("text", shared)
                    }
                }
                invoke.resolve(out)
            } catch (e: Exception) {
                invoke.reject("couldn't read the file: ${e.message}")
            }
        }
    }

    private fun displayName(uri: Uri): String? {
        if (uri.scheme == "file") return uri.lastPathSegment
        return activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
            if (it.moveToFirst()) it.getString(0) else null
        }
    }

    // a WireGuard config is a few hundred bytes; anything big is something else
    private fun read(uri: Uri): String = String(readBytes(uri, 64_000), Charsets.UTF_8)

    private fun readBytes(uri: Uri, max: Int): ByteArray {
        val stream = activity.contentResolver.openInputStream(uri) ?: throw IllegalStateException("no data")
        stream.use { return it.readNBytesCompat(max) }
    }

    private fun java.io.InputStream.readNBytesCompat(max: Int): ByteArray {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(8192)
        while (out.size() < max) {
            val n = read(buf, 0, minOf(buf.size, max - out.size()))
            if (n < 0) break
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }
}
