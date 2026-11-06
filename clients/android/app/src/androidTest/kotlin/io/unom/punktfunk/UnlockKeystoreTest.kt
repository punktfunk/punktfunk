package io.unom.punktfunk

import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.security.KeyFactory
import java.security.Signature
import java.security.spec.X509EncodedKeySpec
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * A profile unlock key in the real Keystore: needs a screen lock and an enrolled fingerprint.
 * The biometric half runs only with `-e unlockSheet 1`, and waits for the sensor
 * (`adb -e emu finger touch 1` on an emulator).
 */
@RunWith(AndroidJUnit4::class)
class UnlockKeystoreTest {
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val keys by lazy { KeystoreUnlockKeys(context) }
    private val host = "cd".repeat(32)
    private val message = ByteArray(152) { it.toByte() }

    @After
    fun forget() = keys.delete(host, "test")

    @Test
    fun a_key_is_its_p256_point_and_signs_only_behind_the_sheet() {
        assumeTrue("no strong biometric enrolled", keys.canEnroll())
        val point = keys.create(host, "test")!!
        assertEquals(65, point.size)
        assertEquals(4, point[0].toInt())
        assertTrue(keys.has(host, "test"))
        // No biometric match yet: the hardware refuses the signature.
        val bare = keys.signer(host, "test")!!
        assertTrue(runCatching { bare.update(message); bare.sign() }.isFailure)

        assumeTrue("run with -e unlockSheet 1, then touch the sensor", arg("unlockSheet") == "1")
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            lateinit var activity: MainActivity
            scenario.onActivity { activity = it }
            val signed = runBlocking(Dispatchers.Main) {
                withTimeout(90_000) { authenticateSigner(activity, keys.signer(host, "test")!!, "Test") }
            }!!
            signed.update(message)
            val proof = signed.sign()
            // The exported point is the hardware key: it verifies the hardware's signature.
            val pub = KeyFactory.getInstance("EC").generatePublic(X509EncodedKeySpec(SPKI_P256 + point))
            val verify = Signature.getInstance("SHA256withECDSA").apply { initVerify(pub); update(message) }
            assertTrue(verify.verify(proof))
        }
        keys.delete(host, "test")
        assertFalse(keys.has(host, "test"))
    }

    /** `-e invalidate make`, enrol another fingerprint, then `-e invalidate check`. */
    @Test
    fun a_new_fingerprint_invalidates_the_key() {
        val step = arg("invalidate")
        assumeTrue("run with -e invalidate make, then check", step != null)
        val kept = "ef".repeat(32)
        if (step == "make") {
            assertTrue(keys.create(kept, "test") != null)
            return
        }
        assertNull(keys.signer(kept, "test"))
        assertFalse(keys.has(kept, "test"))
    }

    private fun arg(name: String) = InstrumentationRegistry.getArguments().getString(name)

    private companion object {
        val SPKI_P256 = hexBytes("3059301306072a8648ce3d020106082a8648ce3d030107034200", 26)!!
    }
}
