package io.unom.punktfunk

import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Log
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.Signature
import java.security.cert.CertificateFactory
import java.security.spec.ECGenParameterSpec
import kotlin.coroutines.resume
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext

private const val TAG = "pf.unlock"

/**
 * [UnlockKeys] in the Android Keystore: EC P-256 for SHA256withECDSA, usable only right after a
 * strong biometric match, invalidated when the enrolled biometrics change, in StrongBox where the
 * device has one. Keystore keys never leave the hardware and are never backed up.
 */
class KeystoreUnlockKeys(context: Context) : UnlockKeys {
    private val app = context.applicationContext

    override fun canEnroll(): Boolean =
        BiometricManager.from(app).canAuthenticate(BiometricManager.Authenticators.BIOMETRIC_STRONG) ==
            BiometricManager.BIOMETRIC_SUCCESS

    override fun has(hostFp: String, profile: String): Boolean =
        runCatching { keyStore().containsAlias(alias(hostFp, profile)) }.getOrDefault(false)

    override fun create(hostFp: String, profile: String): ByteArray? {
        delete(hostFp, profile)
        val alias = alias(hostFp, profile)
        val strongBox = app.packageManager.hasSystemFeature(PackageManager.FEATURE_STRONGBOX_KEYSTORE)
        // A HAL can claim StrongBox and still refuse the key; the TEE holds it then.
        val pair = runCatching { generate(alias, strongBox) }
            .recoverCatching { if (strongBox) generate(alias, strongBox = false) else throw it }
            .onFailure { Log.w(TAG, "unlock key not made", it) }
            .getOrNull() ?: return null
        return p256Point(pair.public.encoded) ?: run {
            delete(hostFp, profile)
            null
        }
    }

    override fun delete(hostFp: String, profile: String) {
        runCatching { keyStore().deleteEntry(alias(hostFp, profile)) }
    }

    override fun signer(hostFp: String, profile: String): Signature? = try {
        (keyStore().getKey(alias(hostFp, profile), null) as? PrivateKey)
            ?.let { key -> Signature.getInstance("SHA256withECDSA").apply { initSign(key) } }
    } catch (_: KeyPermanentlyInvalidatedException) {
        delete(hostFp, profile)
        null
    } catch (e: Exception) {
        Log.w(TAG, "unlock key can't sign", e)
        null
    }

    private fun generate(alias: String, strongBox: Boolean): KeyPair {
        val spec = KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_SIGN)
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setUserAuthenticationRequired(true)
            .setInvalidatedByBiometricEnrollment(true)
            .setIsStrongBoxBacked(strongBox)
            .apply {
                // Below API 30 the default already asks for every use, and only a biometric opens such a key.
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
                }
            }
            .build()
        return KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, "AndroidKeyStore")
            .apply { initialize(spec) }
            .generateKeyPair()
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    /** The alias hashes the host's id for the profile, so nothing the host names reaches it. */
    private fun alias(hostFp: String, profile: String): String =
        "punktfunk_unlock_v1_" + MessageDigest.getInstance("SHA-256")
            .digest("${hostFp.lowercase()}\n$profile".toByteArray())
            .joinToString("") { "%02x".format(it) }
}

/**
 * The system's biometric sheet over [activity] for [signature], strong biometrics only and no
 * device-credential fallback: the signature once a biometric matched, else `null` (the player
 * chose the password or the [pin], cancelled, or the sheet failed).
 */
suspend fun authenticateSigner(
    activity: FragmentActivity,
    signature: Signature,
    profileName: String,
    pin: Boolean = false,
): Signature? =
    suspendCancellableCoroutine { cont ->
        val callback = object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                if (cont.isActive) cont.resume(result.cryptoObject?.signature)
            }

            override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                if (cont.isActive) cont.resume(null)
            }
        }
        val prompt = BiometricPrompt(activity, ContextCompat.getMainExecutor(activity), callback)
        val info = BiometricPrompt.PromptInfo.Builder()
            .setTitle(activity.getString(R.string.unlock_title, profileName))
            .setNegativeButtonText(activity.getString(if (pin) R.string.unlock_use_pin else R.string.android_unlock_use_password))
            .setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG)
            .build()
        cont.invokeOnCancellation { activity.runOnUiThread { prompt.cancelAuthentication() } }
        try {
            prompt.authenticate(info, BiometricPrompt.CryptoObject(signature))
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "biometric sheet refused", e)
            if (cont.isActive) cont.resume(null)
        }
    }

/** Whether this shell shows the unlock prompt: the touch UI on every device, a TV's too. */
fun promptsUnlock(context: Context): Boolean = context is FragmentActivity

/**
 * The touch shell's [UnlockHooks] for a dial to [host] as [identity]; `null` where it shows no
 * prompt. [ask] shows the password or PIN prompt. [sheet] is told while the biometric sheet is up.
 */
fun touchUnlockHooks(
    context: Context,
    identity: ClientIdentity,
    host: KnownHost,
    ask: suspend (PasswordAsk) -> PasswordAnswer?,
    sheet: (Boolean) -> Unit = {},
): UnlockHooks? {
    if (!promptsUnlock(context) || host.fpHex.isEmpty()) return null
    val activity = context as FragmentActivity
    val deviceFp = certFingerprint(identity.certPem) ?: return null
    return UnlockHooks(
        keys = KeystoreUnlockKeys(context),
        deviceFp = deviceFp,
        askPassword = ask,
        challenge = { id ->
            withContext(Dispatchers.IO) {
                HostProfiles.unlockChallenge(identity, host.address, host.effectiveMgmtPort, host.fpHex, id, host.port)
            }
        },
        message = NativeBridge::nativeProfileUnlockMessage,
        profileMessage = NativeBridge::nativeLockedProfileUnlockMessage,
        authenticate = { signature, name, pin ->
            sheet(true)
            try {
                authenticateSigner(activity, signature, name, pin)
            } finally {
                sheet(false)
            }
        },
        row = { id ->
            val answer = withContext(Dispatchers.IO) {
                HostProfiles.fetch(identity, host.address, host.effectiveMgmtPort, host.fpHex, host.port)
            }
            (answer as? ProfilesAnswer.Listed)?.rows?.firstOrNull { it.id == id }
        },
    )
}

/** SHA-256 of a PEM certificate's DER: what a host's QUIC handshake knows this device by. */
internal fun certFingerprint(pem: String): ByteArray? = runCatching {
    val der = CertificateFactory.getInstance("X.509").generateCertificate(pem.byteInputStream()).encoded
    MessageDigest.getInstance("SHA-256").digest(der)
}.getOrNull()
