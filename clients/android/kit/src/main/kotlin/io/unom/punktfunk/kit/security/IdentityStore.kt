package io.unom.punktfunk.kit.security

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import java.io.File
import java.security.KeyStore
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ExecutionException
import java.util.concurrent.Future
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import kotlin.concurrent.thread

private const val TAG = "PunktfunkIdentity"

/** The delimiter the JNI uses to join the two PEMs; collision-free (PEM bodies never contain it). */
private const val PEM_DELIM = "\n-----PUNKTFUNK-KEY-----\n"

/** This device's persistent punktfunk identity (presented to hosts via TLS client auth). */
data class ClientIdentity(val certPem: String, val privateKeyPem: String)

/** Result of [IdentityStore.load] — four states so the caller never mints over a *recoverable* error. */
sealed interface IdentityLoad {
    data class Ok(val identity: ClientIdentity) : IdentityLoad

    /** Genuine first run (no blob on disk) — mint a new identity here, and only here. */
    object Absent : IdentityLoad

    /** A blob exists but can't be decrypted (Keystore key gone, corruption). NEVER shadow-mint. */
    data class Unrecoverable(val reason: String, val cause: Throwable?) : IdentityLoad
}

class IdentityUnrecoverableException(message: String, cause: Throwable?) : Exception(message, cause)

/** Split the JNI's joined "<cert>\n-----PUNKTFUNK-KEY-----\n<key>" blob; `null` if malformed. */
fun splitGenerated(joined: String): ClientIdentity? {
    val i = joined.indexOf(PEM_DELIM)
    if (i < 0) return null
    return ClientIdentity(
        certPem = joined.substring(0, i),
        privateKeyPem = joined.substring(i + PEM_DELIM.length),
    )
}

/** Serialises the mint below. Process-wide: the two shells share one file, not one store object. */
private val MINT_LOCK = Any()

/** How long [IdentityHolder] waits on [obtainIdentity]: the keystore answers in ms, a wedge never. */
private const val IDENTITY_OBTAIN_TIMEOUT_MS = 10_000L

// Three in-band attempts absorb a transient KeyMint failure without making the player tap again.
private const val IDENTITY_MINT_ATTEMPTS = 3
private const val IDENTITY_MINT_RETRY_DELAY_MS = 100L

/**
 * Load the device identity, establishing it only on genuine first run. NEVER mints over an error state:
 * an [IdentityLoad.Unrecoverable] surfaces as a throw so the UI can tell the user (re-pair) rather
 * than silently swapping in a new identity (which would change our fingerprint everywhere).
 *
 * [IdentityHolder] runs this on a background thread, and a retry can overlap a wedged call. The
 * lock and re-read make concurrent first-run calls share one persisted identity. A first mint can
 * fail transiently in KeyMint, so the same call retries in-band and re-reads before each attempt;
 * it never mints over a recovered identity.
 */
fun obtainIdentity(store: IdentityStore): ClientIdentity =
    when (val r = store.load()) {
        is IdentityLoad.Ok -> r.identity
        IdentityLoad.Absent -> synchronized(MINT_LOCK) {
            obtainAbsentIdentity(store::load, { mint(store) }) { failure, attempt ->
                Log.w(TAG, "identity mint attempt $attempt did not complete — retrying", failure)
                Thread.sleep(IDENTITY_MINT_RETRY_DELAY_MS)
            }
        }
        is IdentityLoad.Unrecoverable ->
            throw IdentityUnrecoverableException(r.reason, r.cause)
    }

/**
 * The device identity, loaded once per process and shared by both shells. Three states: a load in
 * flight, ready ([current]), or failed — the obtain threw or outlived [timeoutMs], which a wedged
 * keystore does. A guarded action that finds no identity shows [blockedMessage], which re-kicks a
 * failed load, so the tap that reports the failure is also its retry.
 */
class IdentityHolder internal constructor(
    private val obtain: () -> ClientIdentity,
    private val timeoutMs: Long = IDENTITY_OBTAIN_TIMEOUT_MS,
    private val onFailure: (Throwable) -> Unit = { Log.w(TAG, "identity unavailable", it) },
    /** Words message [id]. The process's holder reads the app's resources. */
    private val words: (id: Int) -> String = { it.toString() },
) {
    @Volatile
    var current: ClientIdentity? = null
        private set

    /** The last load ended without an identity. Stays set while a retry is in flight. */
    @Volatile
    var failed = false
        private set

    /** The first load has ended, with or without an identity. */
    val settled: Boolean get() = current != null || failed

    private var pending: Future<ClientIdentity?>? = null

    /** Start a load unless one is ready or in flight. The future settles within [timeoutMs]. */
    @Synchronized
    fun ensure(): Future<ClientIdentity?> {
        current?.let { return CompletableFuture.completedFuture(it) }
        pending?.let { return it }
        val obtained = FutureTask(obtain)
        val waited = FutureTask {
            val id = try {
                obtained.get(timeoutMs, TimeUnit.MILLISECONDS)
            } catch (e: Exception) {
                onFailure((e as? ExecutionException)?.cause ?: e)
                null
            }
            settle(id)
            id
        }
        pending = waited
        thread(isDaemon = true, name = "pf-identity") { obtained.run() }
        thread(isDaemon = true, name = "pf-identity-wait") { waited.run() }
        return waited
    }

    /** [ensure], then wait for it: the identity, or null once the load failed. Blocks. */
    fun await(): ClientIdentity? = ensure().get()

    /** The line a guarded action shows while [current] is null. Re-kicks a failed load. */
    fun blockedMessage(): String {
        if (!failed) return words(R.string.android_identity_not_ready)
        ensure()
        return words(R.string.android_identity_broken)
    }

    @Synchronized
    private fun settle(id: ClientIdentity?) {
        current = id
        failed = id == null
        pending = null
    }

    companion object {
        @Volatile
        private var instance: IdentityHolder? = null

        /** The process's one holder. */
        fun shared(context: Context): IdentityHolder =
            instance ?: synchronized(this) {
                instance ?: context.applicationContext.let { app ->
                    IdentityHolder({ obtainIdentity(IdentityStore(app)) }, words = { app.getString(it) })
                }.also { instance = it }
            }
    }
}

/** Retry a genuine first-run mint, re-reading before every attempt so a persisted identity wins. */
internal fun obtainAbsentIdentity(
    load: () -> IdentityLoad,
    mint: () -> ClientIdentity,
    beforeRetry: (failure: Exception, failedAttempt: Int) -> Unit = { _, _ -> },
): ClientIdentity {
    var lastFailure: Exception? = null
    repeat(IDENTITY_MINT_ATTEMPTS) { index ->
        when (val current = load()) {
            is IdentityLoad.Ok -> return current.identity
            is IdentityLoad.Unrecoverable ->
                throw IdentityUnrecoverableException(current.reason, current.cause)
            IdentityLoad.Absent -> try {
                return mint()
            } catch (failure: Exception) {
                lastFailure = failure
                if (index + 1 < IDENTITY_MINT_ATTEMPTS) beforeRetry(failure, index + 1)
            }
        }
    }
    throw checkNotNull(lastFailure)
}

/** Generate and persist a fresh identity. Call under [MINT_LOCK]. */
private fun mint(store: IdentityStore): ClientIdentity {
    val id = splitGenerated(NativeBridge.nativeGenerateIdentity())
        ?: throw IdentityUnrecoverableException("nativeGenerateIdentity returned empty", null)
    store.persist(id)
    return id
}

/**
 * Persists the identity PEM blob to app-private storage, wrapped with an AndroidKeyStore AES-256-GCM
 * key (never exportable; StrongBox-backed where available, TEE otherwise). On-disk layout:
 * `[12-byte IV][GCM ciphertext+tag]`. The wrapping key never leaves the secure element, and Keystore
 * keys don't survive backup/restore — so a restored device reads [IdentityLoad.Absent] (the blob is
 * excluded from backup; see the manifest) and re-mints, rather than carrying a dead identity.
 */
class IdentityStore(context: Context) {
    private val appCtx = context.applicationContext
    private val file = File(appCtx.filesDir, "pf_identity.bin")
    private val alias = "punktfunk_identity_v1"

    fun load(): IdentityLoad {
        if (!file.exists()) return IdentityLoad.Absent
        return try {
            val blob = file.readBytes()
            if (blob.size <= IV_LEN) {
                return IdentityLoad.Unrecoverable("identity blob truncated (${blob.size} B)", null)
            }
            val key = (keyStore().getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.secretKey
                ?: return IdentityLoad.Unrecoverable("blob present but Keystore key missing", null)
            val iv = blob.copyOfRange(0, IV_LEN)
            val ct = blob.copyOfRange(IV_LEN, blob.size)
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, iv))
            val plain = String(cipher.doFinal(ct), Charsets.UTF_8)
            splitGenerated(plain)?.let { IdentityLoad.Ok(it) }
                ?: IdentityLoad.Unrecoverable("decrypted identity blob malformed", null)
        } catch (e: Exception) {
            // Decrypt/Keystore failure: the identity is unrecoverable. Do NOT mint a shadow identity.
            Log.e(TAG, "identity load failed", e)
            IdentityLoad.Unrecoverable("identity decrypt failed: ${e.javaClass.simpleName}", e)
        }
    }

    fun persist(identity: ClientIdentity) {
        val key = getOrCreateKey()
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val iv = cipher.iv // GCM: a fresh random 12-byte IV per encryption
        val plain = (identity.certPem + PEM_DELIM + identity.privateKeyPem).toByteArray(Charsets.UTF_8)
        val ct = cipher.doFinal(plain)
        // Write to a temp file then rename, so a crash mid-write can't leave a torn (unrecoverable) blob.
        val tmp = File(file.parentFile, "${file.name}.tmp")
        tmp.writeBytes(iv + ct)
        if (!tmp.renameTo(file)) {
            file.writeBytes(iv + ct)
            tmp.delete()
        }
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private fun getOrCreateKey(): SecretKey {
        val ks = keyStore()
        (ks.getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        // Prefer a StrongBox-backed key; fall back to TEE on ANY keygen failure — a HAL that
        // reports StrongBox but answers with ProviderException/KeyStoreException instead of
        // StrongBoxUnavailableException must not wedge first-run mint.
        return try {
            generateKey(strongBox = true)
        } catch (e: Exception) {
            Log.i(TAG, "StrongBox keygen failed — using TEE-backed key", e)
            generateKey(strongBox = false)
        }
    }

    private fun generateKey(strongBox: Boolean): SecretKey {
        val spec = KeyGenParameterSpec.Builder(
            alias,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setIsStrongBoxBacked(strongBox)
            .build()
        val kg = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        kg.init(spec)
        return kg.generateKey()
    }

    private companion object {
        const val TRANSFORM = "AES/GCM/NoPadding"
        const val IV_LEN = 12
        const val GCM_TAG_BITS = 128
    }
}
