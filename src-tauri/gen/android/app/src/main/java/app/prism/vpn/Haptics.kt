package app.prism.vpn

import android.app.Activity
import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.view.HapticFeedbackConstants
import android.view.View
import kotlin.math.max
import kotlin.math.min
import kotlin.math.pow

/**
 * The page's haptics. Taps, ticks and toggles are the system's own click
 * feedback (they follow the phone's touch-feedback setting and feel native);
 * the orb's hold-to-charge is drawn by hand on the vibrator.
 */
object Haptics {
    private val sdk = Build.VERSION.SDK_INT

    fun play(activity: Activity, view: View?, kind: String, ms: Int) {
        try {
            when (kind) {
                "charge" -> charge(activity, ms.coerceIn(200, 5000))
                "burst" -> burst(activity)
                "cancel" -> vibrator(activity)?.cancel()
                else -> view?.let { v -> activity.runOnUiThread { v.performHapticFeedback(constant(kind)) } }
            }
        } catch (_: Exception) {
            // a phone without a vibrator or a vendor quirk: haptics are a nicety
        }
    }

    private fun constant(kind: String): Int = when (kind) {
        "tick" -> if (sdk >= 34) HapticFeedbackConstants.SEGMENT_TICK else HapticFeedbackConstants.CLOCK_TICK
        "on" -> if (sdk >= 34) HapticFeedbackConstants.TOGGLE_ON else HapticFeedbackConstants.CONTEXT_CLICK
        "off" -> when {
            sdk >= 34 -> HapticFeedbackConstants.TOGGLE_OFF
            sdk >= 27 -> HapticFeedbackConstants.VIRTUAL_KEY_RELEASE
            else -> HapticFeedbackConstants.VIRTUAL_KEY
        }
        "success" -> if (sdk >= 30) HapticFeedbackConstants.CONFIRM else HapticFeedbackConstants.LONG_PRESS
        "error" -> if (sdk >= 30) HapticFeedbackConstants.REJECT else HapticFeedbackConstants.LONG_PRESS
        "heavy" -> HapticFeedbackConstants.LONG_PRESS
        else -> HapticFeedbackConstants.VIRTUAL_KEY // "tap"
    }

    private fun vibrator(ctx: Context): Vibrator? {
        val v = if (sdk >= 31) {
            ctx.getSystemService(VibratorManager::class.java)?.defaultVibrator
        } else {
            @Suppress("DEPRECATION")
            ctx.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator
        }
        return v?.takeIf { it.hasVibrator() }
    }

    /**
     * Charging up over [ms]: soft, slow ticks that come faster and harder
     * until they run together into a buzz, like something spinning up.
     */
    private fun charge(ctx: Context, ms: Int) {
        val v = vibrator(ctx) ?: return
        v.cancel()
        if (sdk >= 31 && v.areAllPrimitivesSupported(
                VibrationEffect.Composition.PRIMITIVE_LOW_TICK,
                VibrationEffect.Composition.PRIMITIVE_TICK,
            )
        ) {
            // crisp primitives where the phone has them
            val c = VibrationEffect.startComposition()
            var t = 0
            var n = 0
            while (t < ms && n < 40) {
                val p = t.toFloat() / ms
                val gap = max(18, (150 * (1 - p).pow(1.6f)).toInt())
                val prim = if (p < 0.45f) VibrationEffect.Composition.PRIMITIVE_LOW_TICK else VibrationEffect.Composition.PRIMITIVE_TICK
                c.addPrimitive(prim, min(1f, 0.2f + 0.8f * p * p + 0.1f), if (n == 0) 0 else gap)
                t += gap + 12
                n++
            }
            v.vibrate(c.compose())
            return
        }
        val timings = ArrayList<Long>()
        val amps = ArrayList<Int>()
        var t = 0
        while (t < ms) {
            val p = t.toFloat() / ms
            val on = (9 + 16 * p).toInt()
            val off = 6 + (130 * (1 - p) * (1 - p)).toInt()
            timings += on.toLong(); amps += (35 + 220 * p * p).toInt().coerceIn(1, 255)
            timings += off.toLong(); amps += 0
            t += on + off
        }
        vibrate(v, timings, amps)
    }

    /** The release: one heavy hit with a rumble that dies away. */
    private fun burst(ctx: Context) {
        val v = vibrator(ctx) ?: return
        v.cancel()
        val timings = arrayListOf<Long>(55)
        val amps = arrayListOf(255)
        var a = 210.0
        while (a > 8) {
            timings += 16; amps += a.toInt()
            a *= 0.78
        }
        vibrate(v, timings, amps)
    }

    /** A waveform with amplitudes where the motor can do them, plain on/off otherwise. */
    private fun vibrate(v: Vibrator, timings: List<Long>, amps: List<Int>) {
        if (sdk >= 26 && v.hasAmplitudeControl()) {
            v.vibrate(VibrationEffect.createWaveform(timings.toLongArray(), amps.toIntArray(), -1))
            return
        }
        // on/off only: keep the strong parts, as [wait, buzz, wait, buzz, ...]
        val pattern = ArrayList<Long>()
        var wait = 0L
        for (i in timings.indices) {
            if (amps[i] >= 110) {
                pattern += wait; pattern += timings[i]; wait = 0
            } else {
                wait += timings[i]
            }
        }
        if (pattern.isEmpty()) return
        if (sdk >= 26) {
            v.vibrate(VibrationEffect.createWaveform(pattern.toLongArray(), -1))
        } else {
            @Suppress("DEPRECATION")
            v.vibrate(pattern.toLongArray(), -1)
        }
    }
}
