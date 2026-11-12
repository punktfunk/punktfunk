// In-memory brute-force throttle for the login gate. The password compare is constant-time (no
// timing leak), but nothing bounded the *volume* of guesses — a LAN peer could hammer
// PUNKTFUNK_UI_PASSWORD at network speed against a console that is, by design, LAN-exposed. This is
// the console analogue of the host's PAIRING_COOLDOWN / single-use pairing PIN: every other secret
// gate in the project is rate-limited, so this one is too.
//
// State is a module-level Map — it lives for the lifetime of the single Bun/Nitro server process,
// which is exactly the scope we want (a restart clearing it is fine; lockouts are short). Keyed on
// the socket peer IP (NOT a spoofable X-Forwarded-For — see the caller). The map is size-capped so a
// distinct-IP flood can't grow it without bound, and a global counter adds a mild floor so a
// botnet-style spread across many IPs still can't get unlimited aggregate guesses.

/** Backoff schedule. After `FREE_ATTEMPTS` failures from an IP, each further failure locks that IP
 * for `BASE_LOCKOUT_MS * 2^(n)` up to `MAX_LOCKOUT_MS`. Generous enough never to bother a human who
 * fat-fingered their password a couple of times, punishing enough to make brute force hopeless. */
const FREE_ATTEMPTS = 5;
const BASE_LOCKOUT_MS = 1_000;
const MAX_LOCKOUT_MS = 5 * 60_000; // 5 minutes
/** Forget an IP that's been quiet this long (also the idle-eviction horizon). */
const ENTRY_TTL_MS = 15 * 60_000;
/** Cap the tracking map so a distinct-IP flood can't grow memory without bound. */
const MAX_TRACKED_IPS = 4_096;
/** Global floor: once this many failures accumulate inside GLOBAL_WINDOW_MS across ALL IPs, every
 * login attempt waits out a small fixed delay too, so a spread-out flood is still bounded. */
const GLOBAL_FAIL_THRESHOLD = 100;
const GLOBAL_WINDOW_MS = 60_000;
const GLOBAL_LOCK_MS = 1_000;

interface Entry {
	fails: number;
	/** Epoch ms before which this IP may not attempt again. */
	lockedUntil: number;
	/** Last activity (for TTL eviction). */
	seen: number;
}

const byIp = new Map<string, Entry>();
let globalFails: number[] = []; // epoch-ms timestamps of recent failures (any IP)

function sweep(now: number): void {
	if (byIp.size <= MAX_TRACKED_IPS) {
		// Cheap path: only drop clearly-stale entries when we're not under memory pressure.
		for (const [ip, e] of byIp) {
			if (now - e.seen > ENTRY_TTL_MS) byIp.delete(ip);
		}
		return;
	}
	// Over the cap: evict oldest-seen first until back under it.
	const oldest = [...byIp.entries()].sort((a, b) => a[1].seen - b[1].seen);
	for (const [ip, e] of oldest) {
		if (byIp.size <= MAX_TRACKED_IPS && now - e.seen <= ENTRY_TTL_MS) break;
		byIp.delete(ip);
	}
}

/** Call before the first await of a password check. If the IP (or the global floor) is locked
 * out, returns the ms the caller should tell the client to wait. Otherwise counts this attempt as
 * a failure and returns 0, so parallel guesses arm the lockout as sequential ones do. A match
 * then clears the IP's count with `recordLoginSuccess`; the global floor keeps it. */
export function takeAttempt(ip: string, now = Date.now()): number {
	const e = byIp.get(ip);
	if (e && now < e.lockedUntil) return e.lockedUntil - now;
	globalFails = globalFails.filter((t) => now - t < GLOBAL_WINDOW_MS);
	if (globalFails.length >= GLOBAL_FAIL_THRESHOLD) return GLOBAL_LOCK_MS;
	recordLoginFailure(ip, now);
	return 0;
}

/** Record a failed attempt and arm/extend this IP's lockout with exponential backoff. */
function recordLoginFailure(ip: string, now: number): void {
	sweep(now);
	const e = byIp.get(ip) ?? { fails: 0, lockedUntil: 0, seen: now };
	e.fails += 1;
	e.seen = now;
	const over = e.fails - FREE_ATTEMPTS;
	if (over > 0) {
		const lock = Math.min(MAX_LOCKOUT_MS, BASE_LOCKOUT_MS * 2 ** (over - 1));
		e.lockedUntil = now + lock;
	}
	byIp.set(ip, e);
	globalFails.push(now);
}

/** Record a successful login — clears the IP's failure state so a legitimate user isn't penalized
 * for earlier typos. */
export function recordLoginSuccess(ip: string): void {
	byIp.delete(ip);
}
