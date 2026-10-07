// Capture marketing/console screenshots from the built Storybook.
//
// Mirrors the iOS harness (clients/apple/tools/screenshots.sh): one "scene" per
// story, a mock-populated REAL view, captured by the platform's own renderer —
// here headless Chromium over `storybook-static`. No display, GPU, login, or live
// mgmt backend: the page stories render entirely from fixtures (src/stories/lib).
//
//   bun run build-storybook            # produce ./storybook-static
//   node tools/screenshots.mjs         # → ./screenshots/<story-id>.png
//
// Env knobs: OUT (output dir), STORYBOOK_STATIC (input dir), SETTLE (ms after the
// page looks ready, default 600), WIDTH/HEIGHT/SCALE (viewport, default 1440x900@2x),
// ONLY (comma-separated story-id substring filter), GLOBALS (Storybook globals,
// `density:flush`; also suffixes the file name), FULL=1 (the whole page, not the viewport).

import { existsSync } from "node:fs";
import { mkdir, readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, join, normalize, resolve } from "node:path";
import { chromium } from "playwright";

const ROOT = resolve(process.env.STORYBOOK_STATIC ?? "storybook-static");
const OUT = resolve(process.env.OUT ?? "screenshots");
const SETTLE = Number(process.env.SETTLE ?? 600);
const WIDTH = Number(process.env.WIDTH ?? 1440);
const HEIGHT = Number(process.env.HEIGHT ?? 900);
const SCALE = Number(process.env.SCALE ?? 2);
const GLOBALS = process.env.GLOBALS ?? "";
const FULL = process.env.FULL === "1";
const ONLY = (process.env.ONLY ?? "")
	.split(",")
	.map((s) => s.trim())
	.filter(Boolean);

// Only the page-level + shell stories make sense as console screenshots — skip the
// component-library stories (Button, Badge, …).
const TITLE_PREFIXES = ["Pages/", "Shell/"];

const MIME = {
	".html": "text/html",
	".js": "text/javascript",
	".mjs": "text/javascript",
	".css": "text/css",
	".json": "application/json",
	".svg": "image/svg+xml",
	".png": "image/png",
	".jpg": "image/jpeg",
	".woff": "font/woff",
	".woff2": "font/woff2",
	".ttf": "font/ttf",
	".map": "application/json",
	".ico": "image/x-icon",
};

function staticServer(rootDir) {
	return createServer(async (req, res) => {
		try {
			const url = new URL(req.url, "http://localhost");
			let path = decodeURIComponent(url.pathname);
			if (path.endsWith("/")) path += "index.html";
			// Contain the path to rootDir (no traversal).
			const filePath = normalize(join(rootDir, path));
			if (!filePath.startsWith(rootDir)) {
				res.writeHead(403).end();
				return;
			}
			const body = await readFile(filePath);
			res.writeHead(200, {
				"content-type": MIME[extname(filePath)] ?? "application/octet-stream",
			});
			res.end(body);
		} catch {
			res.writeHead(404).end();
		}
	});
}

async function listStories(rootDir) {
	const indexPath = join(rootDir, "index.json");
	if (!existsSync(indexPath)) {
		throw new Error(
			`${indexPath} not found — run \`bun run build-storybook\` first`,
		);
	}
	const index = JSON.parse(await readFile(indexPath, "utf8"));
	const entries = Object.values(index.entries ?? index.stories ?? {});
	return entries
		.filter((e) => e.type === "story" || e.type === undefined)
		.filter((e) => TITLE_PREFIXES.some((p) => (e.title ?? "").startsWith(p)))
		.filter((e) => ONLY.length === 0 || ONLY.some((f) => e.id.includes(f)))
		.sort((a, b) => a.id.localeCompare(b.id));
}

async function main() {
	if (!existsSync(ROOT)) {
		throw new Error(
			`${ROOT} not found — run \`bun run build-storybook\` first`,
		);
	}
	const stories = await listStories(ROOT);
	if (stories.length === 0)
		throw new Error("no Pages/* or Shell/* stories found");
	await mkdir(OUT, { recursive: true });

	const server = staticServer(ROOT);
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	const port = server.address().port;

	const browser = await chromium.launch({
		args: ["--force-color-profile=srgb"],
	});
	const context = await browser.newContext({
		viewport: { width: WIDTH, height: HEIGHT },
		deviceScaleFactor: SCALE,
		colorScheme: "dark",
	});

	let ok = 0;
	for (const story of stories) {
		const page = await context.newPage();
		const url = `http://127.0.0.1:${port}/iframe.html?id=${encodeURIComponent(
			story.id,
		)}&viewMode=story${GLOBALS ? `&globals=${encodeURIComponent(GLOBALS)}` : ""}`;
		try {
			await page.goto(url, { waitUntil: "networkidle", timeout: 30_000 });
			// Story root mounted with real content.
			await page.waitForSelector("#storybook-root > *", { timeout: 20_000 });
			// Web fonts settled (else text reflows / falls back in the shot).
			await page.evaluate(() => document.fonts.ready);
			// Recharts mounts behind a client-only guard — wait for the SVG if present.
			await page
				.locator(".recharts-surface")
				.first()
				.waitFor({ timeout: 4_000 })
				.catch(() => {});
			await page.waitForTimeout(SETTLE);
			const suffix = GLOBALS ? `--${GLOBALS.replace(/[^a-z0-9]+/gi, "-")}` : "";
			const file = join(OUT, `${story.id}${suffix}.png`);
			await page.screenshot({ path: file, fullPage: FULL });
			console.log(`✓ ${story.id} → ${file}`);
			ok++;
		} catch (e) {
			console.warn(`✗ ${story.id}: ${e.message}`);
		} finally {
			await page.close();
		}
	}

	await browser.close();
	await new Promise((r) => server.close(r));
	console.log(`\n${ok}/${stories.length} stories captured → ${OUT}`);
	if (ok === 0) process.exit(1);
}

main().catch((e) => {
	console.error(e);
	process.exit(1);
});
