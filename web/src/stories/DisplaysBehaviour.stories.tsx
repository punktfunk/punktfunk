import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
	BehaviourPicker,
	CustomiseDialog,
} from "@/sections/Displays/Behaviour";
import {
	displayCustomPresets,
	displayEffective,
	displayPolicy,
	displayPresets,
} from "./lib/fixtures";

/**
 * How this host treats a device that connects (design/web-console-overhaul.md D4, §5.3).
 *
 * The preset grid is a motion container (`components/stagger.tsx`), and this is where a reviewer
 * sees its cadence. Each `PresetCard` is a `motion.div` that carries its own `ROW` values; the
 * container carries only the variant NAME and the gap between siblings. Nesting one motion
 * element inside another without re-declaring the gap is what silently flattens it, and nothing
 * in the types catches that.
 */
const noop = () => {};

const meta = {
	title: "Console/Displays Behaviour",
	parameters: { layout: "padded" },
} satisfies Meta;
export default meta;

type Story = StoryObj<typeof meta>;

/** The picker as the page shows it: every preset readable, the current one ringed. */
export const Picker: Story = {
	render: () => (
		<div className="max-w-4xl">
			<BehaviourPicker
				policy={displayPolicy}
				presets={displayPresets}
				customPresets={displayCustomPresets}
				onApply={noop}
				onCustomise={noop}
				onSavePreset={noop}
				onRenamePreset={noop}
				onUpdatePreset={noop}
				onDeletePreset={noop}
			/>
		</div>
	),
};

/** A host with no saved bundles — five built-ins and the way into the questions. */
export const NoCustomPresets: Story = {
	render: () => (
		<div className="max-w-4xl">
			<BehaviourPicker
				policy={displayPolicy}
				presets={displayPresets}
				customPresets={[]}
				onApply={noop}
				onCustomise={noop}
				onSavePreset={noop}
				onRenamePreset={noop}
				onUpdatePreset={noop}
				onDeletePreset={noop}
			/>
		</div>
	),
};

/**
 * The questions, which arrive on the same cadence. Each `Question` is a `motion.fieldset` that
 * declares its own `QUESTION` values; a plain `fieldset` brings none, and the group would not
 * animate — the failure is silent and looks exactly like a container that was never wrapped.
 */
export const Customise: Story = {
	render: function CustomiseStory() {
		const [open, setOpen] = useState(true);
		return (
			<div className="max-w-4xl">
				<Button onClick={() => setOpen(true)}>Open</Button>
				<CustomiseDialog
					open={open}
					onOpenChange={setOpen}
					effective={displayEffective}
					policy={displayPolicy}
					enforced={[
						"keep_alive",
						"topology",
						"mode_conflict",
						"identity",
						"game_session",
					]}
					onSetField={noop}
				/>
			</div>
		);
	},
};
