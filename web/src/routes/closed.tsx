import { createFileRoute } from "@tanstack/react-router";
import { CLOSED, type Closed } from "@/lib/person";
import { PersonClosed } from "@/sections/Person/Closed";

// Where a person's console says it did not open, or closed. Public: a sentence and nothing else.
export const Route = createFileRoute("/closed")({
	validateSearch: (s: Record<string, unknown>): { why: Closed } => ({
		why: (CLOSED as readonly unknown[]).includes(s.why)
			? (s.why as Closed)
			: "ended",
	}),
	component: RouteComponent,
});

function RouteComponent() {
	const { why } = Route.useSearch();
	return <PersonClosed why={why} />;
}
