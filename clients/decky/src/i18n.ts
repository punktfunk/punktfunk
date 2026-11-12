// The panel's words. `virtual:l10n` is built by rollup.config.js from l10n/messages: the
// messages the code names, and the host's `reject_` messages, for the shipped languages.
import bundle from "virtual:l10n";
import { type Params, pick, say } from "./l10n";

// Steam names its languages by API name. `window.LocalizationManager.m_rgLocalesToUse` lists
// the ones its own UI reads, most preferred first. A name not here is tried as a language tag.
const STEAM_LANGUAGES: Record<string, string> = {
  arabic: "ar", brazilian: "pt-BR", bulgarian: "bg", czech: "cs", danish: "da", dutch: "nl",
  english: "en", finnish: "fi", french: "fr", german: "de", greek: "el", hungarian: "hu",
  indonesian: "id", italian: "it", japanese: "ja", koreana: "ko", latam: "es-419",
  norwegian: "nb", polish: "pl", portuguese: "pt-PT", romanian: "ro", russian: "ru",
  schinese: "zh-Hans", spanish: "es", swedish: "sv", tchinese: "zh-Hant", thai: "th",
  turkish: "tr", ukrainian: "uk", vietnamese: "vi",
};

/** The language Steam shows, else the browser's, as tags for `pick`. */
function osLanguages(): string[] {
  const steam: unknown = (window as any).LocalizationManager?.m_rgLocalesToUse;
  const names: unknown[] =
    Array.isArray(steam) && steam.length > 0 ? steam : [...(navigator.languages ?? [])];
  return names
    .filter((n): n is string => typeof n === "string")
    .map((n) => STEAM_LANGUAGES[n.toLowerCase()] ?? n);
}

const language = pick(osLanguages(), bundle.shipped, bundle.fallback);

/** The message `key` in the player's language. */
export const t = (key: string, params?: Params): string => say(bundle, language, key, params);

/** The host's refusal `token` (`not-armed`) in words; null for a token this build lacks. */
export function rejectText(token: string | undefined): string | null {
  const key = `reject_${(token ?? "").replace(/-/g, "_")}`;
  return token && key in bundle.messages[bundle.source] ? t(key) : null;
}
