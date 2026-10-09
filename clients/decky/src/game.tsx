// What the game page knows about its title, and the brand pieces drawn for it.
import { toaster } from "@decky/api";
import { FC } from "react";
import { startGameStream } from "./hooks";
import type { HostView } from "./hosts";

/** What the game page knows about its title; what the per-game shortcut is dressed with. */
export interface Game {
  appId: number;
  title: string;
  iconHash: string;
}

export function streamFrom(host: HostView, game: Game): void {
  // A sleeping host is the one case that takes a while and looks like nothing happened.
  if (!host.online) {
    toaster.toast({ title: "Punktfunk", body: `Waking ${host.name} to stream ${game.title}` });
  }
  void startGameStream(host, game.appId, game.title, game.iconHash);
}

/** The Punktfunk lens mark: the logo's two overlapping circles in the brand violets, the lighter
 *  one behind. A surface that paints itself violet overrides `--pf-back` / `--pf-deep`, because the
 *  deep circle is the same violet as the Play button's focus fill and would vanish on it. */
export const PunktfunkMark: FC<{ size?: number }> = ({ size = 22 }) => (
  <svg viewBox="17 13 141 141" width={size} height={size} aria-hidden="true">
    <defs>
      <linearGradient id="pf-lens" x1="0" y1="1" x2="1" y2="0">
        <stop offset="0" stopColor="#ffffff" stopOpacity="0" />
        <stop offset="1" stopColor="#ffffff" stopOpacity="0.9" />
      </linearGradient>
    </defs>
    <circle cx="65.44" cy="105.85" r="44.3" style={{ fill: "var(--pf-back, #a79ff8)" }} />
    <circle cx="109.74" cy="61.55" r="44.3" style={{ fill: "var(--pf-deep, #6c5bf3)" }} />
    <path
      fill="url(#pf-lens)"
      d="M121.228,104.359c-14.777,3.965 -31.187,0.136 -42.811,-11.488c-11.624,-11.624 -15.453,-28.034 -11.488,-42.811c14.777,-3.965 31.187,-0.136 42.811,11.488c11.624,11.624 15.453,28.034 11.488,42.811Z"
    />
  </svg>
);
