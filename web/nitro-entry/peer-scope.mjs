// Who may reach the console: the same address table the host's `classify_source`
// (crates/host/punktfunk-host/src/native_pairing/approval.rs) reads for a pairing knock.
import { createSocket } from "node:dgram";
import { isIPv4, isIPv6 } from "node:net";
import { networkInterfaces } from "node:os";

/**
 * True for a peer on this machine, the local network or a Tailscale or NetBird network: loopback,
 * RFC 1918, link-local, IPv6 unique-local and link-local, and a 100.64/10 peer `overMesh` confirms.
 * That range is carrier-grade NAT space too, so the address alone proves nothing. A v4-mapped v6
 * address is judged as the v4 address it carries. Anything else, including an address we could
 * not read, is false.
 */
export async function isLocalPeer(address, overMesh = routesOverMesh) {
	const ip = address?.split("%")[0].toLowerCase();
	if (ip && isIPv4(ip)) return isLocalV4(ip.split(".").map(Number), overMesh);
	if (!ip || !isIPv6(ip)) return false;
	const h = hextets(ip);
	if (h.slice(0, 5).every((x) => x === 0) && h[5] === 0xffff) {
		return isLocalV4(
			[h[6] >> 8, h[6] & 0xff, h[7] >> 8, h[7] & 0xff],
			overMesh,
		);
	}
	const loopback = h.slice(0, 7).every((x) => x === 0) && h[7] === 1;
	return loopback || (h[0] & 0xffc0) === 0xfe80 || (h[0] & 0xfe00) === 0xfc00;
}

function isLocalV4(octets, overMesh) {
	const [a, b] = octets;
	return (
		a === 127 ||
		a === 10 ||
		(a === 172 && b >= 16 && b < 32) ||
		(a === 192 && b === 168) ||
		(a === 169 && b === 254) ||
		(isCgnat(octets) && overMesh(octets.join(".")))
	);
}

/** RFC 6598 shared space, 100.64/10: a mesh VPN's addresses and a CGNAT ISP's subscribers alike. */
function isCgnat([a, b]) {
	return a === 100 && b >= 64 && b < 128;
}

/**
 * True when this machine routes its replies to `ip` out of a mesh VPN's interface. A TCP
 * handshake needs those replies, so a CGNAT neighbour that borrows a mesh address never
 * completes one. A UDP `connect` looks the route up without sending; any failure is false.
 */
export async function routesOverMesh(ip) {
	const sock = createSocket("udp4");
	try {
		await new Promise((resolve, reject) => {
			sock.once("error", reject);
			sock.connect(9, ip, resolve);
		});
		const src = sock.address().address;
		return Object.entries(networkInterfaces()).some(
			([name, addrs]) =>
				addrs?.some((a) => a.address === src) && isMeshInterface(name, src),
		);
	} catch {
		return false;
	} finally {
		sock.close();
	}
}

/**
 * A mesh VPN's interface: Tailscale's `tailscale0` on Linux and `Tailscale` on Windows, NetBird's
 * `wt0` on both, a `utun` on macOS. No ISP puts a 100.64/10 address on any of those names.
 */
export function isMeshInterface(name, address) {
	return (
		/^(tailscale|utun|wt0$)/i.test(name) &&
		isIPv4(address) &&
		isCgnat(address.split(".").map(Number))
	);
}

/** The eight 16-bit groups of a valid IPv6 literal, `::` and a dotted v4 tail expanded. */
function hextets(ip) {
	const tail = ip.match(/(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
	let s = ip;
	if (tail) {
		const [a, b, c, d] = tail.slice(1).map(Number);
		s = `${ip.slice(0, tail.index)}${((a << 8) | b).toString(16)}:${((c << 8) | d).toString(16)}`;
	}
	const [left, right] = s.split("::");
	const head = left ? left.split(":") : [];
	const rest = right ? right.split(":") : [];
	const gap =
		right === undefined ? [] : Array(8 - head.length - rest.length).fill("0");
	return [...head, ...gap, ...rest].map((x) => Number.parseInt(x, 16));
}
