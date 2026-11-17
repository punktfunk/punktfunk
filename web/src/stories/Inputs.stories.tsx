import type { Meta, StoryObj } from "@storybook/react-vite";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

const meta = {
	title: "UI/Inputs",
	component: Input,
} satisfies Meta<typeof Input>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Form: Story = {
	render: () => (
		<div className="max-w-sm space-y-4">
			<div className="space-y-1.5">
				<Label htmlFor="host">Host address</Label>
				<Input id="host" placeholder="192.168.1.173" />
			</div>
			<div className="space-y-1.5">
				<Label htmlFor="pin">Pairing PIN</Label>
				<Input id="pin" inputMode="numeric" maxLength={4} placeholder="0000" />
			</div>
			<div className="space-y-1.5">
				<Label htmlFor="disabled">Disabled</Label>
				<Input id="disabled" disabled placeholder="unavailable" />
			</div>
		</div>
	),
};

/**
 * The non-text controls, side by side with an Input — the comparison that matters, because the
 * failure mode these wrappers exist to prevent is a control that looks borrowed from another app.
 * A raw `<select>` / `<textarea>` / `<input type="checkbox">` renders in the browser's own chrome
 * and ignores the brand tokens entirely.
 */
export const Controls: Story = {
	render: () => (
		<div className="max-w-sm space-y-4">
			<div className="space-y-1.5">
				<Label htmlFor="ctl-event">Event</Label>
				<Select defaultValue="session.started">
					<SelectTrigger id="ctl-event">
						<SelectValue />
					</SelectTrigger>
					<SelectContent>
						<SelectItem value="session.started">session.started</SelectItem>
						<SelectItem value="session.ended">session.ended</SelectItem>
						<SelectItem value="game.running">game.running</SelectItem>
					</SelectContent>
				</Select>
			</div>
			<div className="space-y-1.5">
				<Label htmlFor="ctl-cmd">Command</Label>
				<Input id="ctl-cmd" placeholder="/usr/local/bin/on-stream.sh" />
			</div>
			<div className="space-y-1.5">
				<Label htmlFor="ctl-paths">Extra library folders</Label>
				<Textarea
					id="ctl-paths"
					className="h-24 font-mono text-xs"
					placeholder={"/mnt/games\n/mnt/roms"}
				/>
			</div>
			<div className="flex items-center gap-2">
				<Checkbox id="ctl-launcher" defaultChecked />
				<Label htmlFor="ctl-launcher">This entry opens a launcher</Label>
			</div>
		</div>
	),
};

/** The select with its list open — the surface, the tick, and the hover highlight. */
export const SelectOpen: Story = {
	render: () => (
		<div className="max-w-sm">
			<Select defaultValue="session.started" open>
				<SelectTrigger aria-label="Event">
					<SelectValue />
				</SelectTrigger>
				<SelectContent>
					<SelectItem value="client.connected">client.connected</SelectItem>
					<SelectItem value="session.started">session.started</SelectItem>
					<SelectItem value="session.ended">session.ended</SelectItem>
					<SelectItem value="game.running">game.running</SelectItem>
				</SelectContent>
			</Select>
		</div>
	),
};
