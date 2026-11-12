import type { FC } from "react";
import { useProfileView } from "@/api/seat";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

/** In a profile's view: the titles are the owner's library's, and the way there. */
export const SharedNote: FC = () => {
	const view = useProfileView();
	if (!view) return null;
	return (
		<p className="max-w-prose text-sm text-muted-foreground">
			{m.profile_view_shared({ owner: view.ownerName })}{" "}
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
