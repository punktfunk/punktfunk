import type { Meta, StoryObj } from "@storybook/react-vite";
import { useEffect, useState } from "react";
import { useDialogs } from "@/components/dialogs";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages";

/**
 * The console's confirm/prompt, in place of `window.confirm` / `window.prompt`.
 *
 * A native dialog cannot be storied: it is browser chrome, outside the page and outside any
 * screenshot.
 *
 * `open` fires the dialog on mount so the screenshot harness catches it without an interaction.
 */
const Demo = ({
	open,
	kind,
}: {
	open: boolean;
	kind: "destructive" | "plain" | "prompt";
}) => {
	const { confirm, promptText } = useDialogs();
	const [answer, setAnswer] = useState<string>("—");

	const ask = async () => {
		if (kind === "prompt") {
			const name = await promptText({
				title: m.display_preset_save_title(),
				label: m.display_preset_name(),
				defaultValue: "Couch (TV only)",
			});
			setAnswer(name ?? "cancelled");
			return;
		}
		const ok = await confirm(
			kind === "destructive"
				? {
						title: m.library_delete_confirm(),
						description: m.library_delete_body(),
						confirmLabel: m.library_delete(),
						destructive: true,
					}
				: {
						title: m.display_preset_delete_confirm(),
						confirmLabel: m.display_preset_delete(),
					},
		);
		setAnswer(String(ok));
	};

	// biome-ignore lint/correctness/useExhaustiveDependencies: fire once, on mount, for the shot
	useEffect(() => {
		if (open) void ask();
	}, [open]);

	return (
		<div className="space-y-4">
			<Button onClick={ask}>Ask</Button>
			<p className="text-sm text-muted-foreground">answered: {answer}</p>
		</div>
	);
};

// No `DialogsProvider` decorator here on purpose: `.storybook/preview.tsx` mounts it for every
// story, exactly as `__root` does for every route. A second one would work but would quietly render
// a second dialog host.
const meta = {
	title: "UI/Dialogs",
	component: Demo,
	args: { open: true, kind: "destructive" },
} satisfies Meta<typeof Demo>;

export default meta;
type Story = StoryObj<typeof meta>;

/** Anything that destroys or removes: red affirmative, and the consequence spelled out under it. */
export const Destructive: Story = {};

/** A plain choice — no red, because nothing is lost that the operator did not already choose. */
export const Plain: Story = { args: { kind: "plain" } };

/** The prompt: a real labelled field, autofocused, Enter to submit. */
export const Prompt: Story = { args: { kind: "prompt" } };
