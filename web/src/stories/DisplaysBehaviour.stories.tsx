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
 * This story is back on purpose. The last one was deleted when the presets moved behind a
 * [ Change ] button — "the grid, its stagger and the tab shell it hung from all went with the
 * draft machinery" — and both defects it existed to catch came straight back: the five answers
 * to the page's central question were invisible until you found a small button, and once found
 * they all landed on a single frame.
 *
 * So the grid is a motion container again (`components/stagger.tsx`), and this is where a
 * reviewer sees the cadence. `Card` and `Button` carry their own `from`/`enter` values; the
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
 * The questions, which arrive on the same cadence. A `fieldset` is not a motion element and
 * brings no values of its own, so this group only animates because each question declares them —
 * the failure mode is silent and looks exactly like a container that was never wrapped.
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
