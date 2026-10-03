// Which of the host's two identities the console serves — resolved HERE because this entry is the
// one place every launcher routes through.
//
// The host keeps two identities side by side (crate::identity, the "identity split"):
//
//   native-cert.pem / native-key.pem  ECDSA P-256, with real SANs (the machine hostname,
//                                     localhost, 127.0.0.1, ::1). This is what the native QUIC
//                                     plane and the management API present, and what native
//                                     clients pin.
//   cert.pem / key.pem                the legacy RSA GameStream identity: CN=punktfunk and NO SAN
//                                     at all (gamestream::cert::generate passes rcgen an empty SAN
//                                     list), kept byte-stable because Moonlight pins it and the
//                                     pairing hashes bind its X.509 signature bytes.
//
// Every launcher names the LEGACY pair — scripts/punktfunk-web.service, the NixOS module, the
// Windows service supervisor, web-run.cmd, the Steam Deck installer — because they were written
// before the split, and none of them CAN choose: systemd `Environment=` has no "this file, else
// that one". Serving the legacy pair costs twice:
//
//   * a CN-only, SAN-less cert is rejected outright by every current browser
//     (ERR_CERT_COMMON_NAME_INVALID / SSL_ERROR_BAD_CERT_DOMAIN), so the console the operator was
//     told to open does not load;
//   * the tray's loopback liveness probe pins whatever the mgmt API serves — the NATIVE cert — so
//     the handshake is refused and a perfectly healthy console is labelled "Open web console (not
//     responding)" while the host beside it reads "idle" (field report 2026-08-24).
//
// So prefer the native sibling. It is also the smaller secret to hand a bundled bun: on a default
// build key.pem is the Moonlight PAIRING SIGNING key, native-key.pem is only a TLS key.
//
// Swapped as a PAIR or not at all — a native cert with the legacy key is a server that cannot
// complete a handshake with anyone, so both halves must be present AND must come from the same
// directory. An older host still serving the RSA pair to native clients has no native pair on disk
// and falls through unchanged, as does a cert an operator supplied under any other name.
import { statSync } from "node:fs";

/**
 * The directory prefix (separator included) of a path ending in `base`, or null if it does not.
 *
 * Deliberately NOT `node:path`: that resolves per-RUNTIME, so a POSIX build reads
 * `C:\ProgramData\punktfunk\cert.pem` as one long filename — and Windows, where the service
 * supervisor hands us exactly that (windows/service.rs), is the platform CI can never exercise.
 * A suffix test gives the same answer everywhere. It also leaves the prefix VERBATIM, where
 * `join(dirname(p), …)` would normalise `/a/b/../cert.pem` to a different directory than the one
 * the operator named — which matters the moment `b` is a symlink.
 *
 * @param {string} p
 * @param {string} base
 * @returns {string | null}
 */
function dirPrefix(p, base) {
	if (p === base) return ""; // bare relative name
	if (!p.endsWith(base)) return null;
	const sep = p[p.length - base.length - 1];
	return sep === "/" || sep === "\\" ? p.slice(0, -base.length) : null;
}

/**
 * A readable, NON-EMPTY file. Emptiness matters: `pf_paths::write_secret_file` is
 * create+truncate+write rather than temp+rename, so a console starting mid-write could otherwise
 * adopt a 0-byte cert and leave `Bun.serve` throwing on every restart — and not every launcher
 * retries forever (the Steam Deck unit is `Restart=on-failure` under the default rate limit).
 *
 * @param {string} p
 */
function usable(p) {
	try {
		return statSync(p).size > 0;
	} catch {
		return false;
	}
}

/**
 * @param {string | undefined} cert  PUNKTFUNK_UI_TLS_CERT, verbatim.
 * @param {string | undefined} key   PUNKTFUNK_UI_TLS_KEY, verbatim.
 * @param {(p: string) => boolean} [exists]  injected by the test; defaults to a real stat.
 * @returns {{cert: string | undefined, key: string | undefined}}
 */
export function resolveUiTlsPaths(cert, key, exists = usable) {
	// Half-configured TLS is the caller's error to report (it refuses to start); don't mask it by
	// resolving one half of a pair that isn't there.
	if (!cert || !key) return { cert, key };
	const dir = dirPrefix(cert, "cert.pem");
	// Same directory, or we are not looking at a pair — see the PAIR note above.
	if (dir === null || dir !== dirPrefix(key, "key.pem")) return { cert, key };
	const nativeCert = `${dir}native-cert.pem`;
	const nativeKey = `${dir}native-key.pem`;
	return exists(nativeCert) && exists(nativeKey)
		? { cert: nativeCert, key: nativeKey }
		: { cert, key };
}
