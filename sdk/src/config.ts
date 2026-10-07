// Connection resolution (RFC §7): loopback URL + bearer token + the host's self-signed
// identity cert, from the environment with file fallbacks — so `connect()` on the host machine
// needs zero configuration.
//
//   PUNKTFUNK_MGMT_URL     else <config_dir>/mgmt-endpoint (the URL the host actually bound,
//                          rewritten on every start), else https://127.0.0.1:47990
//   PUNKTFUNK_MGMT_TOKEN   (admin override), else PUNKTFUNK_PLUGIN_TOKEN,
//                          else <config_dir>/plugin-token, else <config_dir>/mgmt-token
//   PUNKTFUNK_MGMT_CA      (path; else <config_dir>/native-cert.pem, else cert.pem when present)
//
// Token precedence is deliberate: the host mints a capability-limited `plugin-token` for the
// scripting runner (it cannot register hooks or administer pairing), and that is what a plugin's
// zero-config connect() should hold — the full-admin `mgmt-token` is only a fallback for hosts
// that predate the plugin token (and on Windows the runner's LocalService principal can't read it
// at all). A script that legitimately needs the admin surface sets PUNKTFUNK_MGMT_TOKEN or passes
// { token } explicitly.
//
// The CA is the host's own identity certificate — trusting exactly it (not the system roots)
// IS the pin for the loopback hop. Per-runtime plumbing differs: Bun takes `tls.ca` on fetch,
// Node (undici) takes a dispatcher with a CA-carrying TLS connector; anything else falls back
// to plain fetch (document PUNKTFUNK_MGMT_CA + NODE_EXTRA_CA_CERTS there).
import type { Connection } from "./connection.js";
import { staticBearer } from "./credential.js";
import { socketFetch } from "./pipe-fetch.js";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

export interface ConnectOptions {
	/** Management API base URL (default `https://127.0.0.1:47990`). */
	url?: string;
	/** Bearer token (default: `PUNKTFUNK_MGMT_TOKEN`, else the host's `mgmt-token` file). */
	token?: string;
	/** PEM of the CA to trust — the host's identity cert (default: `PUNKTFUNK_MGMT_CA`, else `native-cert.pem`, else `cert.pem`). */
	ca?: string;
}

/**
 * A Node-resolved connection: a [`Connection`] plus the bearer it was built from, kept because
 * the log shipper and the runner read it directly.
 */
export interface ResolvedConfig extends Connection {
	token: string;
}

/** The host's config dir — the same resolution the host itself uses. */
export const configDir = (): string => {
	const explicit = process.env.PUNKTFUNK_CONFIG_DIR;
	if (explicit) return explicit;
	if (process.platform === "win32") {
		const base = process.env.ProgramData ?? process.env.APPDATA ?? ".";
		return path.join(base, "punktfunk");
	}
	const base =
		process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), ".config");
	return path.join(base, "punktfunk");
};

/**
 * The writable state directory a plugin should persist its config/cache into:
 * `<config_dir>/plugin-state[/<name>]`.
 *
 * WHY this and not `<config_dir>/<name>` directly: on Windows the managed runner is de-privileged
 * (runs as `NT AUTHORITY\LocalService`), and the config dir is locked to Users-read — so a plugin
 * writing straight under it fails with EPERM. `punktfunk-host plugins enable` grants the runner
 * **Modify** on exactly `plugin-state` (the config dir and the plugin *code* stay read-only), so
 * this is the one place a supervised plugin can write. On Linux the runner is a `systemd --user`
 * unit owning the whole config dir, so the path is writable there too — same code, no branch.
 *
 * `name` is a plugin's own kebab-case id; omit it for the shared root. The directory is NOT created
 * here (the caller decides permissions/timing) — `fs.mkdirSync(pluginStateDir(name), {recursive:
 * true})` from the runner inherits the granted ACL on Windows.
 */
export const pluginStateDir = (name?: string): string => {
	const root = path.join(configDir(), "plugin-state");
	return name ? path.join(root, name) : root;
};

/**
 * The ingest inbox a plugin reads data DROPPED BY ANOTHER ACCOUNT from:
 * `<config_dir>/ingest[/<name>]`.
 *
 * The mirror of {@link pluginStateDir}, and the answer to a problem the de-privileging creates on
 * Windows: the LocalService runner can no longer traverse the interactive user's profile, so a
 * plugin can't read a file an app running as *you* produced (e.g. the Playnite exporter's library
 * JSON under your `%APPDATA%`). `punktfunk-host plugins enable` grants `BUILTIN\Users` **write** on
 * exactly `ingest` — so your app drops `ingest/<plugin>/…` and the runner reads it there. On Linux
 * the runner is a `systemd --user` unit owning the config dir, so a same-user producer writes here
 * with no special step.
 *
 * The dir is NOT created here (a producer running as the interactive user creates its own
 * `ingest/<name>` subdir under the host-granted `ingest`). Treat anything read from it as
 * lower-trust than your own state: the inbox is writable by any local user.
 */
export const pluginIngestDir = (name?: string): string => {
	const root = path.join(configDir(), "ingest");
	return name ? path.join(root, name) : root;
};

const readIfExists = (p: string): string | undefined => {
	try {
		return fs.readFileSync(p, "utf8");
	} catch {
		return undefined;
	}
};

/** First token-looking line of the mgmt-token file (tolerates `TOKEN=`-style and blank lines). */
const parseTokenFile = (raw: string): string | undefined => {
	for (const line of raw.split(/\r?\n/)) {
		const t = line.trim();
		if (t.length === 0 || t.startsWith("#")) continue;
		return t.includes("=") ? t.slice(t.indexOf("=") + 1).trim() : t;
	}
	return undefined;
};

/**
 * The mgmt URL the host published: `<config_dir>/mgmt-endpoint`, one
 * `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>` line the host rewrites on every start with the port
 * it REALLY bound. This is how a `PUNKTFUNK_MGMT_BIND` move (the supported way to share a box with
 * Sunshine/Apollo, whose web UI owns 47990) reaches a plugin: the runner is a scheduled task /
 * systemd unit that inherits nothing from `host.env` (which on Windows it can't even read), so
 * before this a moved port left every plugin — and the runner's own log shipper — dialing
 * `127.0.0.1:47990` forever, silently. Field report 2026-08-18. `undefined` when the file is absent
 * (an old host, or a plugin CLI run on another machine); the caller falls back to the default.
 */
export const publishedMgmtUrl = (): string | undefined =>
	parseTokenFile(readIfExists(path.join(configDir(), "mgmt-endpoint")) ?? "");

export const resolveConfig = async (
	options?: ConnectOptions,
): Promise<ResolvedConfig> => {
	const url = (
		options?.url ??
		process.env.PUNKTFUNK_MGMT_URL ??
		publishedMgmtUrl() ??
		"https://127.0.0.1:47990"
	).replace(/\/+$/, "");
	// Never falls back to the admin `mgmt-token` file. On Linux the runner shares the operator's
	// uid, so reading it is one line away — and a zero-config `connect()` that silently picked it
	// up handed every plugin full admin on any host whose plugin token was missing.
	const token =
		options?.token ??
		process.env.PUNKTFUNK_MGMT_TOKEN ??
		process.env.PUNKTFUNK_PLUGIN_TOKEN ??
		parseTokenFile(readIfExists(path.join(configDir(), "plugin-token")) ?? "");
	if (!token) {
		throw new Error(
			"no plugin token: the host writes one to " +
				`${path.join(configDir(), "plugin-token")} once the runner is installed. Pass ` +
				"{ token }, or set PUNKTFUNK_MGMT_TOKEN for a script that needs the admin API.",
		);
	}
	const ca = resolveCa(url, options);
	return {
		url,
		token,
		credential: staticBearer(token),
		ca,
		fetch: await makeFetch(ca),
	};
};

/** The certificate to pin for `url`: explicit, then `PUNKTFUNK_MGMT_CA`, then the host's own. */
const resolveCa = (url: string, options?: ConnectOptions): string | undefined => {
	const caPath = process.env.PUNKTFUNK_MGMT_CA;
	return (
		options?.ca ??
		(caPath ? readIfExists(caPath) : undefined) ??
		(url.startsWith("https://")
			? // The mgmt API presents the NATIVE identity when one exists (the host's identity
				// split); `cert.pem` is the legacy identity, still served on hosts that predate it.
				(readIfExists(path.join(configDir(), "native-cert.pem")) ??
				readIfExists(path.join(configDir(), "cert.pem")))
			: undefined)
	);
};

/** The pinned fetch for `url`, for a caller that forwards requests and holds no token of its own. */
export const hostFetch = (url: string, options?: ConnectOptions): Promise<typeof fetch> =>
	makeFetch(resolveCa(url, options));

/**
 * A fetch that PINS `ca` — the host's self-signed identity cert — on this runtime.
 *
 * The pin is chain verification against exactly that certificate (nothing else can pass),
 * with the HOSTNAME check waived: the host identity cert is deliberately CN-only/no-SAN
 * (native clients pin its fingerprint; see `web/nitro-entry/bun-https.mjs` for the same
 * finding), so standard SAN matching would always fail — and it adds nothing when the chain
 * already admits only the one pinned cert.
 */
const makeFetch = async (ca: string | undefined): Promise<typeof fetch> => {
	// Inside a sandbox there is no route to the host's port: the supervisor listens on this
	// socket and forwards over its own pinned connection, so there is nothing to pin in here.
	const unix = process.env.PUNKTFUNK_MGMT_UNIX?.trim();
	if (unix) {
		// A Windows pipe — the host's own, per plugin — is not a socket bun's fetch dials.
		if (unix.startsWith("\\\\.\\pipe\\")) return socketFetch(unix);
		return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			fetch(input, { ...init, unix } as RequestInit)) as typeof fetch;
	}
	if (!ca) return fetch;
	const skipHostname = { checkServerIdentity: () => undefined };
	// Bun: fetch takes node-compatible `tls` options.
	if (typeof (globalThis as Record<string, unknown>).Bun !== "undefined") {
		return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			fetch(input, {
				...init,
				tls: { ca, ...skipHostname },
			} as RequestInit)) as typeof fetch;
	}
	// Node: global fetch is undici — a per-request dispatcher carries the pin.
	try {
		// Optional dependency — declared in package.json optionalDependencies; absent on
		// runtimes that don't need it (the catch below falls back).
		const { Agent } = (await import("undici" as string)) as {
			Agent: new (opts: unknown) => unknown;
		};
		const dispatcher = new Agent({ connect: { ca, ...skipHostname } });
		return ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			fetch(input, { ...init, dispatcher } as RequestInit)) as typeof fetch;
	} catch {
		// Unknown runtime: plain fetch (system trust) — PUNKTFUNK_MGMT_CA via the runtime's
		// own CA mechanism (e.g. NODE_EXTRA_CA_CERTS / --cert) is the documented fallback.
		return fetch;
	}
};
