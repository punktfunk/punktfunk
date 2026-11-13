import type { FC } from "react";
import { useProfileView } from "@/api/seat";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

/**
 * In a profile's view: the shared titles are the owner's library's, and the way there. A profile
 * with games of its own adds those here. In the person's own console there is no way there: the
 * note says who adds the shared games.
 */
export const SharedNote: FC = () => {
	const view = useProfileView();
	if (!view) return null;
	if (view.person)
		return (
			<p className="max-w-prose text-sm text-muted-foreground">
				{m.person_shared_note({ owner: view.ownerName })}
			</p>
		);
	return (
		<p className="max-w-prose text-sm text-muted-foreground">
			{view.ownGames
				? m.profile_view_shared_own({
						owner: view.ownerName,
						name: view.name,
					})
				: m.profile_view_shared({ owner: view.ownerName })}{" "}
			<Button
				variant="link"
				className="h-auto p-0 text-sm"
				onClick={view.toOwner}
			>
				{m.profile_view_open_owner({ owner: view.ownerName })}
			</Button>
		</p>
	);
};
