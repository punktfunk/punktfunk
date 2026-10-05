import { type FC, useState } from "react";
import { cn } from "@/lib/utils";

/** What a profile's picture needs: a `ProfilePublic`, or a `ProfileRef` looked up in the list. */
export interface AvatarProfile {
	id: string;
	display_name: string;
	accent?: string | null;
	avatar?: string | null;
}

/** First letters of the first two words, so "Kid" reads K and "Anna Lena" AL. */
export function initials(name: string): string {
	return name
		.trim()
		.split(/\s+/)
		.slice(0, 2)
		.map((w) => Array.from(w)[0] ?? "")
		.join("")
		.toUpperCase();
}

/**
 * The profile's picture, else its initials on its accent. `version` changes after an upload, so
 * the browser fetches the new picture instead of the one it cached under the same URL.
 */
export const ProfileAvatar: FC<{
	profile: AvatarProfile;
	version?: number;
	className?: string;
}> = ({ profile, version, className }) => {
	const [broken, setBroken] = useState<string | null>(null);
	const src =
		profile.avatar && version
			? `${profile.avatar}?v=${version}`
			: profile.avatar;
	const box = cn(
		"inline-flex size-9 shrink-0 select-none items-center justify-center overflow-hidden rounded-full text-sm font-semibold",
		className,
	);
	if (src && broken !== src) {
		return (
			<img
				src={src}
				alt=""
				className={cn(box, "object-cover")}
				onError={() => setBroken(src)}
			/>
		);
	}
	return (
		<span
			aria-hidden
			className={cn(box, !profile.accent && "bg-muted text-muted-foreground")}
			style={
				profile.accent
					? { backgroundColor: profile.accent, color: "white" }
					: undefined
			}
		>
			{initials(profile.display_name)}
		</span>
	);
};
