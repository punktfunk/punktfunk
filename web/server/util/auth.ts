/**
 * A revocation marker for issued sessions, PERSISTED across restarts.
 *
 * The session is stateless: everything lives inside the sealed cookie, so `session.clear()` only
 * deletes the BROWSER's copy. A cookie captured beforehand stayed valid for its full 7-day TTL —
 * "log out" did not log anything out.
 *
 * The counter has to survive a restart or it does not do its job: an in-memory `let epoch = 1`
 * revokes within one process run, then resets to 1 the next time the service starts, and a cookie
 * captured from that first run is accepted again for the rest of its TTL. (The seal key cannot save
 * us — it is derived from the stable mgmt token, so pre-restart cookies still unseal fine.) So it
 * lives in a file next to the host's own config.
 *
 * Best-effort by design: if the file cannot be read or written the console still works, it just
 * falls back to in-memory revocation for this process. Refusing to log anyone out because a state
 * file is unwritable would be the wrong trade for a LAN console.
 */
const EPOCH_FILE = (): string =>
	process.env.PUNKTFUNK_UI_EPOCH_FILE ??
	join(
		process.env.PUNKTFUNK_CONFIG_DIR ?? join(homedir(), ".config", "punktfunk"),
		"web-session-epoch",
	);

let epochCache: number | null = null;

/** The epoch a new session is stamped with, and the one the gate requires. */
export function sessionEpoch(): number {
	if (epochCache !== null) return epochCache;
	try {
		const raw = readFileSync(EPOCH_FILE(), "utf8").trim();
		const n = Number.parseInt(raw, 10);
		epochCache = Number.isFinite(n) && n > 0 ? n : 1;
	} catch {
		epochCache = 1; // no file yet — first run
	}
	return epochCache;
}

/** Invalidate every session issued so far (what logging out does). */
export function revokeAllSessions(): void {
	const next = sessionEpoch() + 1;
	epochCache = next;
	try {
		mkdirSync(dirname(EPOCH_FILE()), { recursive: true });
		writeFileSync(EPOCH_FILE(), String(next), { mode: 0o600 });
	} catch {
		// Unwritable state dir: the bump still holds for this process, which is the common case
		// (log out, walk away). It is weaker than persisted, and better than refusing to log out.
	}
}

// Shared auth helpers for the Nitro server (the deployed Bun server). Single-user,
// shared-password gate: the user logs in against PUNKTFUNK_UI_PASSWORD_HASH, which sets a SEALED
// (h3 useSession — AES-GCM) cookie; every request is gated by server/middleware/auth.ts.
// The management hop lives in util/mgmt.ts, the path rules in util/paths.ts.
import {
	createHash,
	timingSafeEqual as nodeTimingSafeEqual,
} from "node:crypto";
import {
	mkdirSync,
	readFileSync,
	renameSync,
	unlinkSync,
	writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import {
	getRequestHeader,
	getRequestIP,
	type H3Event,
	type SessionConfig,
} from "h3";
import { mgmtToken } from "./mgmt";
import { browserScheme } from "./pluginOrigin";

export const SESSION_NAME = "pf_session";

/** Set by the Bun entry (nitro-entry/bun-https.mjs) to the real socket peer, after deleting any
 * inbound copy. Keep the name in sync with that file. */
const PEER_IP_HEADER = "x-pf-peer-ip";

/**
 * The requesting peer, as the key for every per-peer budget (currently the login throttle).
 *
 * `getRequestIP()` alone does NOT work under the deployed server: Nitro's `localFetch` builds a
 * synthetic request whose socket carries no `remoteAddress`, so h3 finds nothing and every caller
 * collapses onto one shared bucket — which turned the "per-IP" login throttle into a lockout any
 * LAN peer could trigger for everyone. The Bun entry stamps the real peer into PEER_IP_HEADER
 * (unforgeable: it deletes any client-supplied copy first), so prefer that.
 *
 * `getRequestIP` is kept as the fallback for any other ingress (a plain `node`/dev run), and
 * "unknown" as the last resort — a SHARED bucket, deliberately: an unattributable request must
 * still be rate-limited, and failing open would make brute force unbounded.
 */
export function peerAddress(event: H3Event): string {
	const stamped = getRequestHeader(event, PEER_IP_HEADER)?.trim();
	if (stamped) return stamped;
	return getRequestIP(event) ?? "unknown";
}

/** The salted argon2id hash the login compares against, in modular-crypt form.
 *
 * Unquoted on the way out: the password file writes the value single-quoted so the `$` fields
 * survive a shell-sourced env file (SteamOS `. web.env`), and not every launcher strips them. */
export function uiPasswordHash(): string {
	const raw = process.env.PUNKTFUNK_UI_PASSWORD_HASH?.trim() ?? "";
	return raw.replace(/^(['"])([\s\S]*)\1$/, "$2");
}

/** The LEGACY clear-text password, accepted for one release. Empty once the file is migrated. */
export function uiPassword(): string {
	return process.env.PUNKTFUNK_UI_PASSWORD ?? "";
}

/** Whether a password is configured at all. Either key answers; neither ⇒ auth is MISCONFIGURED
 * and the gate fails closed. */
export function authConfigured(): boolean {
	return uiPasswordHash() !== "" || uiPassword() !== "";
}

/**
 * Check a typed password against what is configured. Constant-time on both branches — the
 * clear-text compare byte-wise, `Bun.password.verify` by construction.
 *
 * The legacy clear text WINS while it is set: an operator who puts a `PUNKTFUNK_UI_PASSWORD=` line
 * back is resetting the password, and that is the whole reset path. A match then migrates the file
 * to a hash, which is the last moment that clear text is readable — so a generated password has to
 * be read off disk before the first login, not after it.
 */
export async function verifyUiPassword(password: string): Promise<boolean> {
	const plain = uiPassword();
	const hash = uiPasswordHash();
	if (plain) {
		if (!timingSafeEqual(password, plain)) return false;
		await migratePasswordFile(plain, hash);
		return true;
	}
	return hash !== "" && (await hashMatches(password, hash));
}

/** `Bun.password.verify`, counting a hash this runtime can't read as "no match" and saying so
 * once. A mangled line must not become an open door, nor a 500 on the login route. */
async function hashMatches(password: string, hash: string): Promise<boolean> {
	try {
		return await Bun.password.verify(password, hash);
	} catch {
		if (!warnedHash) {
			warnedHash = true;
			console.warn(
				"[punktfunk-web] PUNKTFUNK_UI_PASSWORD_HASH is not a hash this server can verify — no password is accepted until it is reset",
			);
		}
		return false;
	}
}
let warnedHash = false;

/** The argon2id surface of the runtime the console is deployed on. Declared here rather than
 * pulled in from @types/bun, which redeclares globals this DOM-targeted config owns. */
declare const Bun: {
	password: {
		hash(password: string, opts: { algorithm: "argon2id" }): Promise<string>;
		verify(password: string, hash: string): Promise<boolean>;
	};
};

/** The env file the launcher loads the password line from, and the one the migration rewrites.
 * SteamOS names `web.env` here; everyone else takes the default. */
function passwordFile(): string {
	return (
		process.env.PUNKTFUNK_UI_PASSWORD_FILE ??
		join(
			process.env.PUNKTFUNK_CONFIG_DIR ??
				join(homedir(), ".config", "punktfunk"),
			"web-password",
		)
	);
}

/** Matches either password key, so the rewrite drops both and re-adds one. */
const PASSWORD_LINE = /^\s*PUNKTFUNK_UI_PASSWORD(_HASH)?\s*=/;
let migrated = false;

/**
 * Swap the clear-text line in the password file for a salted hash, once per process, after a
 * password has actually verified.
 *
 * `existing` is the hash already configured: a clear-text line that matches it is the same password
 * written twice, so only the line goes. Anything else is a password the operator has just set, and
 * every session issued before it is revoked — a reset that left old cookies alive would not be one.
 *
 * A file this process can't rewrite — a read-only store, a password a unit injects with no file
 * behind it — keeps working on the clear text. Refusing the login instead would turn a hardening
 * step into a lockout.
 */
async function migratePasswordFile(
	plain: string,
	existing: string,
): Promise<void> {
	if (migrated) return;
	migrated = true;
	const same = existing !== "" && (await hashMatches(plain, existing));
	let hash = existing;
	if (!same) {
		try {
			hash = await Bun.password.hash(plain, { algorithm: "argon2id" });
		} catch (e) {
			console.warn(
				`[punktfunk-web] couldn't hash the console password — it stays in clear in ${passwordFile()}: ${e}`,
			);
			return;
		}
	}
	const file = passwordFile();
	if (!rewritePasswordFile(file, hash)) return;
	// The login route stamps the session AFTER this, so whoever just typed the password stays in.
	if (!same) revokeAllSessions();
	console.warn(
		`[punktfunk-web] console password stored as a salted hash in ${file} — PUNKTFUNK_UI_PASSWORD is deprecated; reset it by writing a new one back to that file and restarting`,
	);
}

/**
 * Write `hash` into `file` as the only password line, keeping every other line (SteamOS keeps the
 * session secret in the same file). Temp file beside it, then rename: a crash can't leave the
 * operator with no password at all.
 *
 * `false` ⇒ the file is untouched, which the caller treats as "keep serving on the clear text".
 * A file with no clear-text line of ours is one case of that: the value came from somewhere else.
 */
function rewritePasswordFile(file: string, hash: string): boolean {
	let body: string;
	try {
		body = readFileSync(file, "utf8");
	} catch {
		return false;
	}
	if (!body.split("\n").some((l) => PASSWORD_LINE.test(l))) return false;
	const kept = body.split("\n").filter((l) => !PASSWORD_LINE.test(l));
	while (kept.length > 0 && kept[kept.length - 1]?.trim() === "") kept.pop();
	// Single-quoted: the hash carries `$` fields, and this file is sourced by a shell on SteamOS.
	const next = `${[...kept, `PUNKTFUNK_UI_PASSWORD_HASH='${hash}'`].join("\n")}\n`;
	const tmp = `${file}.tmp`;
	try {
		writeFileSync(tmp, next, { mode: 0o600 });
		renameSync(tmp, file);
		return true;
	} catch (e) {
		try {
			unlinkSync(tmp);
		} catch {
			// nothing landed
		}
		console.warn(
			`[punktfunk-web] couldn't rewrite ${file} — the console password stays in clear: ${e}`,
		);
		return false;
	}
}

/**
 * The cookie-sealing key for h3 `useSession` (must be ≥ 32 chars). Precedence:
 *   1. PUNKTFUNK_UI_SECRET — explicit operator override.
 *   2. Derived from the MANAGEMENT TOKEN (a 32-byte / 64-hex CSPRNG value) — the packaged deployment
 *      always has one, so the seal key is high-entropy without any extra config.
 *   3. Only as a last resort (dev/local with no token) derive from the password.
 *
 * Why not (2)→password by default: the password is low-entropy (a human picks it), so a key DERIVED
 * from it turns any captured session cookie into an OFFLINE dictionary oracle — an attacker unseals
 * candidate cookies locally, no server round-trips, so the login throttle can't help. The mgmt token
 * is unguessable, so a cookie sealed under it leaks nothing about the password. (Deriving from the
 * token instead of the password also means changing the password no longer silently invalidates
 * sessions; rotating the mgmt token does — the correct, security-relevant trigger.)
 *
 * (3) takes the HASH when there is one: it carries a random salt, so the same human password no
 * longer gives two boxes the same seal key.
 */
export function sessionConfig(): SessionConfig {
	const explicit = process.env.PUNKTFUNK_UI_SECRET;
	const token = mgmtToken();
	let secret: string;
	if (explicit && explicit.length >= 32) {
		secret = explicit;
	} else if (token) {
		// High-entropy source: the CSPRNG mgmt token. Hash it (never use the raw admin token as the
		// seal key) with a distinct label so the two uses can't be conflated.
		secret = createHash("sha256")
			.update(`punktfunk-session-v1:token:${token}`)
			.digest("hex");
	} else {
		// Last resort (no token configured — dev/local only). A real deployment always has a token
		// and never reaches here.
		secret = createHash("sha256")
			.update(`punktfunk-session-v1:${uiPasswordHash() || uiPassword()}`)
			.digest("hex");
	}
	return {
		name: SESSION_NAME,
		// h3's `useSession` calls this seal key `password` (it's the iron/AES-GCM key, not the login
		// password — see the derivation above).
		password: secret,
		// Bounds a stolen/replayed cookie's lifetime (sets the cookie Max-Age AND the iron
		// seal TTL). 7 days for a single-user console.
		maxAge: 60 * 60 * 24 * 7,
		cookie: {
			httpOnly: true,
			sameSite: "lax",
			path: "/",
			// h3 defaults Secure to true, which browsers DROP over plain http:// (so login
			// silently fails on a LAN HTTP server). Only mark Secure when actually behind TLS.
			//
			// Derived from whether TLS is CONFIGURED, not from `PUNKTFUNK_UI_SECURE` alone
			// (2026-08-05 review L-20). The entry point already refuses the inverse mistake —
			// `PUNKTFUNK_UI_SECURE` without TLS exits rather than serving a console whose cookie
			// the browser will never store — but nothing caught this direction: TLS configured and
			// the flag forgotten shipped a session cookie without `Secure`, which a browser will
			// then also send over a plain-http downgrade. The env var still forces it on for a
			// deploy terminating TLS in front of us (a reverse proxy), where this process sees no
			// cert of its own.
			secure:
				(!!process.env.PUNKTFUNK_UI_TLS_CERT &&
					!!process.env.PUNKTFUNK_UI_TLS_KEY) ||
				/^(1|true)$/i.test(process.env.PUNKTFUNK_UI_SECURE ?? ""),
		},
	};
}

/** Constant-time string comparison (avoids leaking the password via timing). */
export function timingSafeEqual(a: string, b: string): boolean {
	const ab = Buffer.from(a);
	const bb = Buffer.from(b);
	if (ab.length !== bb.length) return false;
	return nodeTimingSafeEqual(ab, bb);
}

/**
 * The origin the browser's address bar shows, for CSRF comparison.
 *
 * `getRequestURL().origin` is the wrong source: Nitro's localFetch builds a
 * synthetic request with no TLS socket, so it reports `http:` on an HTTPS
 * listener. The scheme comes from `browserScheme`, the rule `frame-ancestors` uses.
 */
export function csrfRequestOrigin(o: {
	forwardedProto?: string | null;
	listenerScheme?: "http" | "https" | null;
	requestScheme: string;
	host: string;
}): string {
	const scheme = browserScheme(o);
	try {
		return new URL(`${scheme}://${o.host}`).origin;
	} catch {
		return `${scheme}://${o.host}`;
	}
}

/**
 * Whether a mutating request came from another origin and must be refused.
 *
 * SameSite=Lax still attaches the session cookie to another port on the same
 * host (the plugin-UI origin). A browser that sends `Sec-Fetch-Site` is judged
 * by it alone: the console answers with `Referrer-Policy: no-referrer`, so its
 * own form POSTs carry `Origin: null`. Without Fetch-Site, `Origin` decides; a
 * missing one is curl and is allowed, `null` is an opaque document and is not.
 */
export function isCrossSiteMutation(o: {
	method: string;
	fetchSite?: string | null;
	origin?: string | null;
	requestOrigin: string;
}): boolean {
	const method = o.method.toUpperCase();
	if (method === "GET" || method === "HEAD" || method === "OPTIONS") {
		return false;
	}
	const site = o.fetchSite?.toLowerCase();
	if (site) return site !== "same-origin" && site !== "none";
	const origin = o.origin?.trim();
	if (!origin) return false;
	if (origin === "null") return true;
	try {
		return new URL(origin).origin !== o.requestOrigin;
	} catch {
		return true;
	}
}

export interface SessionData {
	authenticated?: boolean;
	/** The epoch this session was sealed under — see `sessionEpoch`. */
	epoch?: number;
}
