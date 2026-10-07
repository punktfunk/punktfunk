import { Check, Copy } from "lucide-react";
import { type FC, useState } from "react";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

/**
 * One labelled, monospaced value with a copy button: the host address, or the
 * `punktfunk://connect/` link that opens a client straight onto this host and carries its
 * certificate fingerprint, so the device verifies what it reached.
 */
export const CopyRow: FC<{ label: string; value: string }> = ({
	label,
	value,
}) => {
	const [copied, setCopied] = useState(false);
	const copy = async () => {
		try {
			await navigator.clipboard.writeText(value);
			setCopied(true);
			// Revert the affordance rather than leaving a permanent tick, which would stop reading as
			// feedback the second time you press it.
			setTimeout(() => setCopied(false), 1500);
		} catch {
			// Clipboard denied (insecure origin, or the user said no) — the value is on screen and
			// selectable, so there is nothing worth interrupting them about.
		}
	};
	return (
		<div className="space-y-1">
			<p className="text-xs text-muted-foreground">{label}</p>
			<div className="flex items-center gap-2">
				<code className="min-w-0 flex-1 truncate rounded-md bg-muted px-3 py-2 font-mono text-xs">
					{value}
				</code>
				<Button
					variant="outline"
					size="icon"
					aria-label={m.connect_copy()}
					onClick={copy}
				>
					{copied ? (
						<Check className="size-4 text-[var(--success)]" />
					) : (
						<Copy className="size-4" />
					)}
				</Button>
			</div>
		</div>
	);
};
