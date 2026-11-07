// The Displays page's plain summary: what happens to my screens when a device connects. The same
// function captions every preset, so picking one means reading what it will do. It says what the
// page shows: a mirror pin and dedicated game sessions change the answer.
//
// Every sentence is a whole message, put side by side and never filled with another message.
import type { EffectivePolicy, GameSession, KeepAlive } from "@/api/gen/model";
import { m } from "@/paraglide/messages";

export interface DescribeOptions {
	/**
	 * A session is streaming right now. The host applies a policy at the next connect, so this
	 * is the only condition under which the "not yet" tail is true — stating it unconditionally
	 * is what made the old pending note noise on an idle host.
	 */
	live?: boolean;
	/** Name the device instead of "each device" — the per-device sheet (§6.2). */
	deviceName?: string;
	/** A monitor every device mirrors instead of getting a screen of its own. */
	mirror?: string | null;
	gameSession?: GameSession;
}

/** `{Who gets which screen}. {What disconnecting does}. {Second device}. [Games…] [Next connection.]` */
export function describePolicy(
	policy: EffectivePolicy,
	opts: DescribeOptions = {},
): string {
	const parts = [
		firstSentence(policy, opts),
		secondDevice(policy.mode_conflict),
	];
	if (opts.gameSession === "dedicated")
		parts.push(m.display_says_game_dedicated());
	if (opts.live) parts.push(m.display_says_next_connect());
	return parts.join(" ");
}

function firstSentence(policy: EffectivePolicy, opts: DescribeOptions): string {
	// A mirrored monitor is the screen: no screen of its own is made, so nothing is kept.
	if (opts.mirror)
		return opts.deviceName
			? m.display_says_mirror_device({
					name: opts.deviceName,
					connector: opts.mirror,
				})
			: m.display_says_mirror({ connector: opts.mirror });
	return `${screenSentence(policy.topology, opts.deviceName)} ${keptSentence(policy.keep_alive)}`;
}

/**
 * The screen a device is given. A named device is one device, so it never reads as "each"; an
 * idle host has no device yet, so it speaks about the next one.
 */
function screenSentence(
	topology: EffectivePolicy["topology"],
	name: string | undefined,
): string {
	switch (topology) {
		case "extend":
			return name
				? m.display_says_device_extend({ name })
				: m.display_says_each_extend();
		case "primary":
			return name
				? m.display_says_device_primary({ name })
				: m.display_says_each_primary();
		case "exclusive":
			return name
				? m.display_says_device_exclusive({ name })
				: m.display_says_each_exclusive();
		default:
			// `auto` is the host choosing per setup — a headless box has nothing to extend from.
			// Naming a specific outcome here would be a guess the host has not made yet.
			return name
				? m.display_says_device_auto({ name })
				: m.display_says_each_auto();
	}
}

/** What happens to that screen when the device disconnects. */
function keptSentence(keep: KeepAlive): string {
	switch (keep.mode) {
		case "off":
			return m.display_says_on_disconnect_remove();
		case "forever":
			return m.display_says_on_disconnect_forever();
		default:
			return m.display_says_on_disconnect_keep({ seconds: keep.seconds });
	}
}

function secondDevice(conflict: EffectivePolicy["mode_conflict"]): string {
	switch (conflict) {
		case "steal":
			return m.display_says_second_steal();
		case "join":
			return m.display_says_second_join();
		case "reject":
			return m.display_says_second_reject();
		default:
			return m.display_says_second_separate();
	}
}
