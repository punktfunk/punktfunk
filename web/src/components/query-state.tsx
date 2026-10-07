import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { ApiError } from "@/api/fetcher";
import { useSeat } from "@/api/seat";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

interface QueryStateProps {
	isLoading: boolean;
	error: unknown;
	refetch?: () => void;
	children: ReactNode;
}

/** Uniform loading/error wrapper for a query-backed view. */
export function QueryState({
	isLoading,
	error,
	refetch,
	children,
}: QueryStateProps) {
	const seat = useSeat();
	if (isLoading) {
		return (
			<div
				role="status"
				className="flex min-h-40 flex-col items-center justify-center gap-3 text-sm text-muted-foreground"
			>
				<Spinner className="size-8" />
				{m.common_loading()}
			</div>
		);
	}
	if (error) {
		const unauthorized = error instanceof ApiError && error.status === 401;
		// A seat that does not answer is the box's 502; its sentence says so, and Profiles starts it.
		const seatDown =
			seat !== null && error instanceof ApiError && error.status === 502;
		return (
			// `role="alert"` so the failure is announced. The loading branch above already has
			// role="status"; without this, a query that resolved into an error swapped one silent
			// region for another and a screen-reader user was told only that loading had stopped.
			<div
				role="alert"
				className="rounded-lg border border-destructive/40 bg-destructive/5 p-4 text-sm"
			>
				<p className="font-medium text-destructive">
					{unauthorized ? m.common_unauthorized() : m.common_error()}
				</p>
				{seatDown && (
					<p className="mt-1 text-muted-foreground">
						{apiErrorMessage(error)}{" "}
						<Link to="/profiles" className="underline hover:text-foreground">
							{m.seat_open_profiles()}
						</Link>
					</p>
				)}
				{refetch && !unauthorized && (
					<Button
						variant="outline"
						size="sm"
						className="mt-3"
						onClick={() => refetch()}
					>
						{m.common_retry()}
					</Button>
				)}
			</div>
		);
	}
	return <>{children}</>;
}
