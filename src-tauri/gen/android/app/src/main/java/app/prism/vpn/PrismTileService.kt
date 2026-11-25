package app.prism.vpn

import android.app.PendingIntent
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService

/** The Quick Settings tile: one tap connects or disconnects, app open or not. */
class PrismTileService : TileService() {
    companion object {
        const val ACTION_CONNECT = "app.prism.vpn.CONNECT"

        /** Asks the system to redraw the tile after the VPN came up or went down. */
        fun refresh(context: Context) {
            try {
                requestListeningState(context, ComponentName(context, PrismTileService::class.java))
            } catch (_: Exception) {
            }
        }
    }

    override fun onStartListening() {
        show(PrismNative.isRunning())
    }

    private fun show(on: Boolean) {
        val tile = qsTile ?: return
        tile.state = if (on) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) tile.subtitle = if (on) "Connected" else null
        tile.updateTile()
    }

    override fun onClick() {
        if (isLocked) unlockAndRun { toggle() } else toggle()
    }

    private fun toggle() {
        if (PrismNative.isRunning()) {
            PrismVpnService.disconnect(this)
            show(false)
            return
        }
        // Allowed to make a VPN and there is a server: the service connects
        // on its own, no window needed.
        if (VpnService.prepare(this) == null && PrismNative.hasConfig(dataDir.absolutePath)) {
            PrismVpnService.connect(this)
            show(true)
            return
        }
        // Otherwise Prism has to ask first (VPN permission or a config).
        val intent = Intent(this, MainActivity::class.java)
            .setAction(ACTION_CONNECT)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startActivityAndCollapse(
                PendingIntent.getActivity(this, 1, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            )
        } else {
            @Suppress("DEPRECATION", "StartActivityAndCollapseDeprecated")
            startActivityAndCollapse(intent)
        }
    }
}
