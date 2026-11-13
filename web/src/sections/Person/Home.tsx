import Section from "@unom/ui/section";
import type { ComponentProps, FC } from "react";
import { ProfileAvatar } from "@/components/profile-avatar";
import { Card, CardContent } from "@/components/ui/card";
import { useLocale } from "@/lib/i18n";
import type { PersonMe } from "@/lib/person";
import { m } from "@/paraglide/messages";
import {
	GameExitFixed,
	GameExitView,
	useGameExit,
} from "@/sections/Profiles/session";

/** The person's home in their own console: who they are, where, and their game-exit choice. */
export const PersonHome: FC<{ me: PersonMe }> = ({ me }) =>
	me.own ? <PersonHomeView me={me} exit={null} /> : <PersonHomeQuery me={me} />;

const PersonHomeQuery: FC<{ me: PersonMe }> = ({ me }) => (
	<PersonHomeView me={me} exit={useGameExit(me.profile)} />
);

/**
 * The page without its queries. `exit` is the game-exit control's state; `null` on their own
 * account, where a game's exit always returns to their desktop.
 */
export const PersonHomeView: FC<{
	me: PersonMe;
	exit: ComponentProps<typeof GameExitView> | null;
}> = ({ me, exit }) => {
	useLocale();
	const shown =
		!exit?.state || exit.state.enforced.includes("session_on_game_exit");
	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="flex items-center gap-4">
					<ProfileAvatar
						profile={{
							id: me.profile,
							display_name: me.name,
							accent: me.accent,
							avatar: me.avatar,
						}}
						className="size-14 text-lg"
					/>
					<div className="min-w-0 space-y-1">
						<h1 className="truncate text-2xl font-semibold">{me.name}</h1>
						<p className="max-w-prose text-sm text-muted-foreground">
							{me.host
								? m.person_home_line({ host: me.host })
								: m.person_home_line_here()}
						</p>
					</div>
				</div>
				{shown && (
					<Card className="max-w-lg">
						<CardContent className="pt-card">
							{exit ? (
								<GameExitView {...exit} />
							) : (
								<GameExitFixed name={me.name} yours />
							)}
						</CardContent>
					</Card>
				)}
			</div>
		</Section>
	);
};
