// Thin reactive layer over Paraglide, made SSR-safe.
//
// The problem it solves: Paraglide's client strategies (localStorage / preferredLanguage) resolve
// the locale synchronously at load, so a `de` user's FIRST client render used German while the
// server render (which has no localStorage / no request-locale wiring) used the base locale `en` —
// a hydration mismatch plus a flash of English on every full page load.
//
// The fix: take over the READ side via `overwriteGetLocale`, returning a single `rendered` locale
// that starts at `baseLocale`. So the server render AND the client's first (hydration) render agree
// — no mismatch. After hydration, `adoptStoredLocale()` (called once from the root) switches to the
// user's persisted/browser choice: one clean transition instead of a mismatch. Paraglide's strategy
// still PERSISTS the choice (`setLocale` writes localStorage); we only own the read side.
import { useSyncExternalStore } from "react";
import {
	baseLocale,
	isLocale,
	locales,
	localStorageKey,
	overwriteGetLocale,
	setLocale,
} from "@/paraglide/runtime";
import { localeFallback, matchLocale } from "./locale-match";

/** The available locales as a union (`'en' | 'de'`), derived from Paraglide's `locales`. */
export type Locale = (typeof locales)[number];

// The locale every `m.*()` renders in. Never mutated on the server (so SSR is always `baseLocale`,
// identical across concurrent requests); mutated only in the browser, via `changeLocale`.
let rendered: Locale = baseLocale as Locale;
overwriteGetLocale(() => rendered);

const listeners = new Set<() => void>();

/** Switch locale and notify subscribers. Persists via Paraglide's strategy (localStorage). */
export function changeLocale(locale: Locale) {
	rendered = locale;
	// `reload: false` keeps the SPA mounted; we re-render via the store below.
	setLocale(locale, { reload: false });
	if (typeof document !== "undefined") document.documentElement.lang = locale;
	for (const l of listeners) l();
}

/**
 * Adopt the user's persisted (localStorage) or browser (navigator) locale — call ONCE from a root
 * effect, AFTER hydration, so the switch never races the initial render (which must match SSR).
 */
export function adoptStoredLocale() {
	if (typeof window === "undefined") return;
	let target: Locale = baseLocale as Locale;
	const stored = window.localStorage?.getItem(localStorageKey);
	if (stored && isLocale(stored)) {
		target = stored;
	} else {
		const asked = navigator.languages?.length
			? navigator.languages
			: [navigator.language];
		target = matchLocale(asked, locales, localeFallback) ?? target;
	}
	if (target !== rendered) changeLocale(target);
}

function subscribe(cb: () => void) {
	listeners.add(cb);
	return () => listeners.delete(cb);
}

/** Current locale, reactive — components using `m.*` should read this so they re-render. */
export function useLocale(): Locale {
	return useSyncExternalStore(
		subscribe,
		() => rendered,
		() => baseLocale as Locale,
	);
}

export { locales };
