import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
	createApp,
	defineEventHandler,
	type EventHandler,
	toWebHandler,
} from "h3";
import login from "../routes/_auth/login.post";
import { confirmPassword } from "./confirm";

beforeAll(async () => {
	process.env.PUNKTFUNK_UI_PASSWORD_HASH = await Bun.password.hash("hunter2", {
		algorithm: "argon2id",
		memoryCost: 1024,
		timeCost: 1,
	});
});
afterAll(() => {
	delete process.env.PUNKTFUNK_UI_PASSWORD_HASH;
});

const confirm = defineEventHandler(async (event) => {
	await confirmPassword(event, "wrong");
	return {};
});

let peer = 0;

describe.each([
	["login", login],
	["password confirmation", confirm],
] as [string, EventHandler][])("%s", (_name, handler) => {
	test("parallel guesses from one peer get no more tries than sequential ones", async () => {
		const handle = toWebHandler(createApp().use(handler));
		const guess = (ip: string) =>
			handle(
				new Request("http://console/", {
					method: "POST",
					headers: {
						"content-type": "application/json",
						"x-pf-peer-ip": ip,
					},
					body: JSON.stringify({ password: "wrong" }),
				}),
			).then((r) => r.status);

		const one = `10.0.0.${++peer}`;
		let tries = 0;
		while (tries < 50 && (await guess(one)) === 401) tries++;
		expect(tries).toBeGreaterThan(0);
		expect(await guess(one)).toBe(429);

		const burst = `10.0.0.${++peer}`;
		const all = await Promise.all(
			Array.from({ length: 20 }, () => guess(burst)),
		);
		expect(all.filter((s) => s === 401)).toHaveLength(tries);
	});
});
