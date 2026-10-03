import { expect, test } from "bun:test";
import type { Download } from "@/api/gen/model/download";
import { formatBytes, progressLine } from "./Install";

const row = (over: Partial<Download>): Download => ({
	app_id: "custom:a",
	title: "Quail",
	state: "downloading",
	done_bytes: 0,
	started_at: "",
	updated_at: "",
	...over,
});

test("sizes read in decimal units, in the console's language", () => {
	expect(formatBytes(12_345_000_000, "en")).toBe("12.3 GB");
	expect(formatBytes(123_400_000_000, "en")).toBe("123 GB");
	expect(formatBytes(48_000_000, "en")).toBe("48 MB");
	expect(formatBytes(1_500_000_000, "de")).toBe("1,5 GB");
});

test("a download's line carries what the host knows of it", () => {
	expect(
		progressLine(
			row({
				done_bytes: 12.3e9,
				total_bytes: 26e9,
				rate_bps: 48e6,
				eta_s: 240,
			}),
			"en",
		),
	).toBe("12.3 GB of 26 GB · 48 MB/s · about 4 min left");
	expect(progressLine(row({ done_bytes: 3.1e9 }), "en")).toBe("3.1 GB so far");
	expect(
		progressLine(
			row({ state: "paused", done_bytes: 1e9, total_bytes: 2e9, rate_bps: 5 }),
			"en",
		),
	).toBe("1 GB of 2 GB");
});
