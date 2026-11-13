import type { FC } from "react";
import Logo from "@/components/logo";
import { Card, CardHeader } from "@/components/ui/card";
import { useLocale } from "@/lib/i18n";
import type { Closed } from "@/lib/person";
import { m } from "@/paraglide/messages";

/** What happened, then the way back in. */
const TEXT: Record<Closed, () => [string, string]> = {
	refused: () => [m.person_closed_refused(), m.person_closed_again()],
	wait: () => [m.person_closed_wait(), m.person_closed_wait_next()],
	ended: () => [m.person_closed_ended(), m.person_closed_again()],
	stopped: () => [m.person_closed_stopped(), m.person_closed_again()],
	"signed-out": () => [m.person_closed_signed_out(), m.person_closed_again()],
};

/**
 * A person's console that did not open, or closed: one sentence of what happened and the way
 * back, which is always their desktop. Never the operator's login.
 */
export const PersonClosed: FC<{ why: Closed }> = ({ why }) => {
	useLocale();
	const [title, next] = TEXT[why]();
	return (
		<main className="flex min-h-screen flex-col items-center justify-center p-6">
			<div className="mb-8 flex w-[120px]">
				<Logo />
			</div>
			<Card className="h-fit w-full max-w-sm grow-0">
				<CardHeader className="items-start gap-2 text-left">
					<h1 className="text-xl font-semibold leading-tight">{title}</h1>
					<p className="text-sm text-muted-foreground">{next}</p>
				</CardHeader>
			</Card>
		</main>
	);
};
