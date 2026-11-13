// A person's own plugin pages: found through their seat host on their session, served on the
// plugin listener under a path their session signed, and gone with that session.
import { beforeEach, expect, test } from "bun:test";
import { resetPersonState } from "./person";
import {
	ADMIN,
	asDoor,
	boxCalls,
	console_,
	jar,
	local,
	PLUGIN_SECRET,
	pluginCalls,
	SEAT_PLUGIN,
	seatCalls,
	seatSays,
	ticket,
} from "./person.harness";
import { mintPluginPagePath, pluginPage } from "./pluginPage";

asDoor();
const handle = await console_();

beforeEach(() => {
	asDoor();
	resetPersonState();
	for (const calls of [seatCalls, boxCalls, pluginCalls]) calls.length = 0;
});

/** A person's cookies; the box's one sign-in read is forgotten, so each test sees its own calls. */
async function signIn(): Promise<string> {
	const cookie = jar(await handle(local(`/_auth/person?ticket=${ticket()}`)));
	for (const calls of [seatCalls, boxCalls]) calls.length = 0;
	return cookie;
}

/** The person's page path for `id`, as the health probe hands it to their console. */
async function pagePath(cookie: string, id = SEAT_PLUGIN) {
	const res = await handle(local(`/_plugin-health/${id}`, { cookie }));
	return { status: res.status, body: (await res.json()) as { path?: string } };
}

const onPluginListener = (path: string, peer = "127.0.0.1") => {
	const req = local(path);
	req.headers.set("x-pf-listener", "plugin");
	req.headers.set("x-pf-peer-ip", peer);
	return handle(req);
};

test("their plugin's page is found on their seat host and dialed with its own secret", async () => {
	const cookie = await signIn();
	const { status, body } = await pagePath(cookie);
	expect(status).toBe(200);
	expect(body.path).toMatch(
		new RegExp(`^/plugin-ui/${SEAT_PLUGIN}-[0-9a-z]+-[0-9a-f]{32}/$`),
	);
	expect(seatCalls.at(-1)).toMatchObject({
		path: `/api/v1/plugins/${SEAT_PLUGIN}/ui-credential`,
		authorization: `Bearer ${seatSays.token}`,
	});

	const page = await onPluginListener(`${body.path}settings?x=1`);
	expect(page.status).toBe(200);
	expect(await page.text()).toBe("<p>/settings</p>");
	expect(pluginCalls.at(-1)).toMatchObject({
		path: "/settings",
		search: "?x=1",
		authorization: `Bearer ${PLUGIN_SECRET}`,
		cookie: null,
	});
	// Nothing asked the box, and nothing carried the admin token.
	expect(boxCalls).toEqual([]);
	expect(JSON.stringify([...seatCalls, ...pluginCalls])).not.toContain(ADMIN);
});

test("a person's page path is theirs alone: not the operator's, not another computer's, not after they leave", async () => {
	const cookie = await signIn();
	const { body } = await pagePath(cookie);
	const path = body.path ?? "";
	expect(pluginPage(path)).toBeNull();
	expect((await onPluginListener(path, "192.168.1.44")).status).toBe(401);
	expect(
		(await onPluginListener(path.replace(`/${SEAT_PLUGIN}-`, "/other-")))
			.status,
	).toBe(401);
	expect(mintPluginPagePath(SEAT_PLUGIN)).not.toBe(path);

	await handle(local("/_auth/person/end", { method: "POST", cookie }));
	pluginCalls.length = 0;
	expect((await onPluginListener(path)).status).toBe(401);
	expect(pluginCalls).toEqual([]);
});

test("a plugin with no listener opens through their seat host's relay, on their session", async () => {
	const cookie = await signIn();
	const { status, body } = await pagePath(cookie, "relayed");
	expect(status).toBe(200);
	const page = await onPluginListener(`${body.path}settings?x=1`);
	expect(page.status).toBe(200);
	expect(await page.text()).toBe("<p>relayed /settings</p>");
	expect(seatCalls.at(-1)).toMatchObject({
		path: "/api/v1/plugins/relayed/ui/settings",
		search: "?x=1",
		authorization: `Bearer ${seatSays.token}`,
	});
	expect(boxCalls).toEqual([]);
	expect(pluginCalls).toEqual([]);
	expect(JSON.stringify(seatCalls)).not.toContain(ADMIN);
});

test("a plugin their seat host doesn't run opens nothing", async () => {
	const cookie = await signIn();
	const { status } = await pagePath(cookie, "missing");
	expect(status).toBe(502);
	expect(boxCalls).toEqual([]);
});
