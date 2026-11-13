import { afterEach, expect, test } from "bun:test";
import { ApiError } from "@/api/fetcher";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { apiErrorCode, apiErrorMessage } from "./errors";

overwriteGetLocale(() => baseLocale);
afterEach(() => overwriteGetLocale(() => baseLocale));

test("reads the host's error and the BFF's thrown refusal alike", () => {
	expect(apiErrorMessage(new ApiError(409, { error: "busy" }))).toBe("busy");
	const thrown = { error: true, statusCode: 503, statusMessage: "no token" };
	expect(apiErrorMessage(new ApiError(503, thrown, ""))).toBe("no token");
	expect(apiErrorMessage(new ApiError(500, "oops", "Server Error"))).toBe(
		"Server Error",
	);
});

const WRONG = {
	error: "That isn't mia's password.",
	code: "credential_wrong",
	params: { account: "mia" },
};

test("words a coded error from its message, in the console's language", () => {
	expect(apiErrorMessage(new ApiError(401, WRONG))).toBe(
		"That isn't mia's password.",
	);
	overwriteGetLocale(() => "de");
	expect(apiErrorMessage(new ApiError(401, WRONG))).toBe(
		"Das ist nicht das Passwort von mia.",
	);
});

test("shows the host's English for a code this build has no message for", () => {
	overwriteGetLocale(() => "de");
	const unknown = { error: "A newer sentence.", code: "from_a_newer_host" };
	expect(apiErrorMessage(new ApiError(409, unknown))).toBe("A newer sentence.");
	// A code is never a way into any other message.
	const aim = { error: "Plain.", code: "../x", params: {} };
	expect(apiErrorMessage(new ApiError(409, aim))).toBe("Plain.");
});

test("shows the host's English when the host left out a value the message needs", () => {
	overwriteGetLocale(() => "de");
	const bare = {
		error: "That isn't anyone's password.",
		code: "credential_wrong",
	};
	expect(apiErrorMessage(new ApiError(401, bare))).toBe(
		"That isn't anyone's password.",
	);
});

test("hands the code to a caller that branches on it", () => {
	const body = { error: "any words", code: "update_session_active" };
	expect(apiErrorCode(new ApiError(409, body))).toBe("update_session_active");
	expect(apiErrorCode(new ApiError(409, { error: "no code" }))).toBeUndefined();
	expect(apiErrorCode(new ApiError(500, "oops"))).toBeUndefined();
	expect(apiErrorCode(new Error("x"))).toBeUndefined();
});
