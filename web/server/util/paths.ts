// Path rules the gate and the proxies share: which paths need no session, the canonical form a
// denylist matches against, and where a login may send the browser next.

/** Paths reachable WITHOUT a session: the login page, the auth endpoints, and the build's
 * static assets (the login page needs its own CSS/JS, all of which live under /assets/).
 * Everything else — crucially ALL of /api — is gated.
 *
 * Note: do NOT allowlist by file extension. The client assets are all under /assets/, and a
 * generic `*.json` allowlist would expose `/api/v1/openapi.json` (and any future
 * `.json`/`.png` management route) through the proxy unauthenticated. */
export function isPublicPath(pathname: string): boolean {
	if (pathname === "/api" || pathname.startsWith("/api/")) return false; // always gated
	if (pathname === "/login") return true;
	if (pathname.startsWith("/_auth/")) return true;
	if (pathname.startsWith("/assets/")) return true;
	if (pathname === "/favicon.ico" || pathname === "/robots.txt") return true;
	// The web manifest must be fetchable to install the app, and it says nothing a logged-out
	// visitor cannot already see from the login page (name, colours, the brand mark).
	if (pathname === "/manifest.webmanifest") return true;
	return false;
}

/**
 * Collapse a request path to the shape an upstream router will actually see: percent-decoded,
 * with empty (`//`) and `.` segments dropped and `..` resolved. Used to test denylists against
 * something an attacker cannot re-spell — `/api//v1/x`, `/api/./v1/x` and `/api/v1/%78` all reach
 * the same handler, so matching only the literal path is not a security boundary.
 *
 * Decoding is per segment and failure-tolerant: a malformed escape keeps the raw segment rather
 * than throwing, so a bad path degrades to "does not match the canonical form" instead of a 500.
 */
export function normalizePath(pathname: string): string {
	const out: string[] = [];
	for (const raw of pathname.split("/")) {
		let seg = raw;
		try {
			seg = decodeURIComponent(raw);
		} catch {
			// Malformed escape — keep the raw segment.
		}
		if (seg === "" || seg === ".") continue;
		if (seg === "..") {
			out.pop();
			continue;
		}
		out.push(seg);
	}
	return `/${out.join("/")}`;
}

/** Validate a post-login redirect target: a same-origin path only. Resolves `next` against a
 * sentinel origin and keeps it only if it stays same-origin — rejecting absolute (`https://evil.com`),
 * protocol-relative (`//evil.com`) AND backslash/tab variants (`/\evil.com`, which the WHATWG URL
 * parser folds to `//evil.com`) that a plain `startsWith("//")` guard lets through. A path that
 * parses same-origin but serializes as `//…` (`/.//evil.com`) is refused too: the browser reads
 * the returned string as protocol-relative.
 *
 * The login page is never a target: the gate redirects a signed-in visitor off `/login` to this
 * path, so `?next=/login` would bounce between the two until the browser gives up. */
export function safeNextPath(next: string | undefined): string {
	if (!next) return "/";
	try {
		const base = "http://pf.invalid";
		const u = new URL(next, base);
		if (
			u.origin !== base ||
			u.pathname === "/login" ||
			u.pathname.startsWith("//")
		)
			return "/";
		return u.pathname + u.search + u.hash;
	} catch {
		return "/";
	}
}
