// What goes in `Authorization`, and how to get it back when the host says 401.
//
// Two kinds of caller reach the management API and they hold different things. A plugin, the
// runner or the console holds a bearer token the host wrote to a file — that is `staticBearer`,
// and it is every credential the SDK had until now. A browser holds no token and no client
// certificate; it holds a non-extractable P-256 key it paired with, and proves that the same way
// it does on the streaming control stream: the host issues a nonce, the key signs it bound to
// the host's own identity, and the exchange returns a short-lived token. That is `deviceKey`.
//
// The seam is deliberately small — a header, and "drop it" — so the HTTP paths on both surfaces
// stay indifferent to which kind they were given. A refresh is the credential's own business.
//
// Platform-neutral. The signing itself is never here: a `Signer` is handed in by whoever owns
// the key, which in a browser is the wasm glue and nowhere else.

import type { Fetch } from "./connection.js";
import { hexToBytes, rawToDer, toBase64 } from "./ecdsa.js";

/**
 * Something that can prove a paired device without giving up its key.
 *
 * `spki` is the public half as base64 DER — the bytes the host stored the SHA-256 of at pairing.
 * `sign` returns what WebCrypto returns: raw `r || s`; the credential converts to DER, which is
 * what the host verifies.
 */
export interface Signer {
	readonly spki: () => Promise<string>;
	readonly sign: (message: Uint8Array) => Promise<Uint8Array>;
}

export interface Credential {
	/** Which kind, for display and for deciding whether a 401 is worth a retry. */
	readonly kind: "bearer" | "device" | "none";
	/** The full `Authorization` value for the next request. May perform an exchange. */
	readonly header: () => Promise<string>;
	/**
	 * The host refused what `header` produced. Forget it; the next `header` re-earns one. A
	 * no-op for a static token, which cannot be re-earned — that is what `kind` tells a retry
	 * loop.
	 */
	readonly invalidate: () => void;
}

/** A token the caller already holds. */
export const staticBearer = (token: string): Credential => ({
	kind: "bearer",
	header: () => Promise.resolve(`Bearer ${token}`),
	invalidate: () => {},
});

/** No token at all: a plugin on its own pipe, where the connection is the credential. */
export const noCredential = (): Credential => ({
	kind: "none",
	header: () => Promise.resolve(""),
	invalidate: () => {},
});

export interface DeviceKeyOptions {
	/** Management API origin, no trailing slash. */
	readonly url: string;
	/**
	 * SHA-256 of the host's long-lived certificate, lowercase hex: what a signature is bound to,
	 * and what pairing stored. Binding to it means a signature made for one host is useless at
	 * another.
	 */
	readonly hostFingerprint: string;
	readonly signer: Signer;
	/** Defaults to `globalThis.fetch`. A CA-pinning fetch goes here where one exists. */
	readonly fetch?: Fetch;
	/** Seconds before expiry at which the token is treated as gone. Default 60. */
	readonly renewMarginSeconds?: number;
}

/**
 * The exact bytes a device signature covers: context, the channel, then the nonce.
 *
 * The same construction as `punktfunk_core::quic::auth_signed_message`, which is what the host
 * verifies against. Both halves have to agree byte for byte, so this is the one place the SDK
 * writes it and `credential.test.ts` pins it against a known vector.
 */
export const DEVICE_AUTH_CONTEXT = "punktfunk-device-auth-v1:";

export const signedMessage = (
	hostFingerprintHex: string,
	nonceHex: string,
): Uint8Array => {
	const ctx = new TextEncoder().encode(DEVICE_AUTH_CONTEXT);
	const binding = hexToBytes(hostFingerprintHex);
	const nonce = hexToBytes(nonceHex);
	if (binding.length !== 32 || nonce.length !== 32) {
		throw new Error("the binding and the nonce are 32 bytes each");
	}
	const out = new Uint8Array(ctx.length + 64);
	out.set(ctx, 0);
	out.set(binding, ctx.length);
	out.set(nonce, ctx.length + 32);
	return out;
};

/** Thrown by `deviceKey` when the host will not exchange: not paired, stale nonce, or a bad
 *  signature — the host deliberately does not say which. */
export class DeviceRefused extends Error {
	constructor(readonly status: number) {
		super(
			status === 401
				? "this host no longer accepts this device — pair again"
				: `the device token exchange failed (${status})`,
		);
		this.name = "DeviceRefused";
	}
}

/**
 * A paired device key, exchanged for a short-lived token and renewed when the host stops
 * accepting one.
 *
 * The token is held here and nowhere else: it is authority, and one place to drop it is the
 * point. A burst of calls runs one exchange rather than four.
 */
export const deviceKey = (opts: DeviceKeyOptions): Credential => {
	const doFetch: Fetch = opts.fetch ?? ((i, init) => globalThis.fetch(i, init));
	const margin = opts.renewMarginSeconds ?? 60;
	let token: string | null = null;
	let expiresAt = 0;
	let pending: Promise<string> | null = null;

	const exchange = async (): Promise<string> => {
		const challenge = await doFetch(`${opts.url}/api/v1/auth/device/challenge`, {
			method: "POST",
			cache: "no-store",
		});
		if (!challenge.ok) throw new DeviceRefused(challenge.status);
		const { nonce } = (await challenge.json()) as { nonce: string };

		const raw = await opts.signer.sign(signedMessage(opts.hostFingerprint, nonce));
		const res = await doFetch(`${opts.url}/api/v1/auth/device/token`, {
			method: "POST",
			cache: "no-store",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({
				device_key: await opts.signer.spki(),
				nonce,
				// The host verifies ASN.1 DER; WebCrypto signs raw.
				signature: toBase64(rawToDer(raw)),
			}),
		});
		if (!res.ok) throw new DeviceRefused(res.status);
		const grant = (await res.json()) as { token: string; expires_at: number };
		token = grant.token;
		expiresAt = grant.expires_at;
		return grant.token;
	};

	const current = (): Promise<string> => {
		if (token && Date.now() / 1000 < expiresAt - margin) {
			return Promise.resolve(token);
		}
		pending ??= exchange().finally(() => {
			pending = null;
		});
		return pending;
	};

	return {
		kind: "device",
		header: async () => `Bearer ${await current()}`,
		invalidate: () => {
			token = null;
			expiresAt = 0;
		},
	};
};
