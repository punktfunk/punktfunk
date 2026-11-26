import { QrCode } from "lucide-react";
import { type FC, useMemo, useState } from "react";
import { encode } from "uqr";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { m } from "@/paraglide/messages";

/**
 * The `punktfunk://connect/` link that opens a client straight onto this host. It carries the
 * certificate fingerprint, so the device verifies what it reached. No port: clients default to
 * 9777, the same as typing the address by hand.
 */
export const connectLink = (h: HostInfo): string => {
	const params = new URLSearchParams({ host: h.local_ip });
	if (h.fingerprint) params.set("fp", h.fingerprint);
	return `punktfunk://connect/${h.uniqueid}?${params}`;
};

/** `value` as a QR code: dark on light in either theme, because that is how scanners read. */
const Code: FC<{ value: string }> = ({ value }) => {
	const { path, size } = useMemo(() => {
		const qr = encode(value, { ecc: "M", border: 4 });
		let d = "";
		qr.data.forEach((row, y) => {
			row.forEach((dark, x) => {
				if (dark) d += `M${x} ${y}h1v1h-1z`;
			});
		});
		return { path: d, size: qr.size };
	}, [value]);
	return (
		<svg
			viewBox={`0 0 ${size} ${size}`}
			shapeRendering="crispEdges"
			className="mx-auto size-60 rounded-lg bg-white text-black"
			aria-hidden
		>
			<path d={path} fill="currentColor" />
		</svg>
	);
};

/** A button that shows `link` as a QR code, for a phone's camera. */
export const QrButton: FC<{ link: string }> = ({ link }) => {
	const [open, setOpen] = useState(false);
	return (
		<>
			<Button
				variant="outline"
				size="icon"
				aria-label={m.connect_qr()}
				title={m.connect_qr()}
				onClick={() => setOpen(true)}
			>
				<QrCode className="size-4" />
			</Button>
			<Dialog open={open} onOpenChange={setOpen}>
				<DialogContent className="max-w-sm">
					<DialogHeader>
						<DialogTitle>{m.connect_qr_title()}</DialogTitle>
						<DialogDescription>{m.connect_qr_body()}</DialogDescription>
					</DialogHeader>
					<Code value={link} />
				</DialogContent>
			</Dialog>
		</>
	);
};
