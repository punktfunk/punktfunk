import { expect, test } from "bun:test";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import type { SessionSummary } from "@/api/gen/model/sessionSummary";
import { recordingOf } from "./NowRows";

const session = {
	client: "417075f54a8b",
	started_unix: 1_000,
	duration_s: 60,
} as SessionSummary;
const rec = (id: string, client: string, startedS: number) =>
	({ id, client, started_unix_ms: startedS * 1000 }) as CaptureMeta;

test("a session links the recording its device started within it", () => {
	const recordings = [
		rec("other-device", "a7ba70fb4ce8", 1_001),
		rec("before", "417075f54a8b", 900),
		rec("late", "417075f54a8b", 1_030),
		rec("at-start", "417075f54a8b", 995),
		rec("after", "417075f54a8b", 1_100),
	];
	expect(recordingOf(session, recordings)?.id).toBe("at-start");
	expect(recordingOf(session, [recordings[0], recordings[1]])).toBeUndefined();
});
