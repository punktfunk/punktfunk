import type { Meta, StoryObj } from "@storybook/react-vite";
import type { UpdateStatus } from "@/api/gen/model";
import { UpdateCard } from "@/sections/Host/UpdateCard";
import { Routed } from "./lib/routed";

const noop = () => {};
const NOW = Math.floor(Date.now() / 1000);

const base: UpdateStatus = {
	apply: "full",
	available: true,
	channel: "stable",
	channel_hint: "sudo dnf upgrade --refresh punktfunk",
	check_disabled: false,
	current_version: "0.48.0",
	install_kind: "dnf",
	not_published: false,
	last_checked_unix: NOW - 600,
	manifest: {
		version: "0.49.0",
		notes_url: "https://example.invalid/notes",
		published_at: "2026-11-25T10:00:00Z",
		serial: 49,
		stale: false,
	},
};

const card = (data: UpdateStatus, applying = false) => ({
	state: { data, isLoading: false, error: null, refetch: noop },
	onCheck: noop,
	checkBusy: false,
	applying: applying ? { target: "0.49.0", startedAt: Date.now() } : null,
	wait: null,
	onApplied: noop,
	onGiveUp: noop,
});

/** The update card under the Host strip: it shows only when the strip's line is not enough. */
const meta = {
	title: "Pages/Host/Update",
	component: UpdateCard,
	decorators: [
		(Story) => (
			<Routed>
				<Story />
			</Routed>
		),
	],
	args: card(base) as never,
} satisfies Meta<typeof UpdateCard>;

export default meta;
type Story = StoryObj<typeof meta>;

/** One click updates this host. */
export const Available: Story = {};

/** The package manager updates this host: the command, and how to turn one click on. */
export const AvailableByCommand: Story = {
	args: card({
		...base,
		apply: "notify",
		opt_in_hint: "sudo usermod -aG punktfunk-update punktfunk",
	}) as never,
};

/** Downloading, after Update now. */
export const Updating: Story = {
	args: card(
		{
			...base,
			job: {
				stage: "downloading",
				received_bytes: 42_000_000,
				total_bytes: 96_000_000,
				started_unix: NOW - 20,
				target_version: "0.49.0",
			},
		},
		true,
	) as never,
};

/** The last update failed, and the check after it did too. */
export const Failed: Story = {
	args: card({
		...base,
		last_error: "feed answered 502",
		last_result: {
			ok: false,
			from: "0.47.0",
			to: "0.48.0",
			stage: "verify",
			error: "signature did not match",
			finished_unix: NOW - 3600,
			log_path: "/var/lib/punktfunk/logs/update-1.log",
		},
	}) as never,
};

/** Staged on an image-based system: a reboot finishes it. */
export const Staged: Story = {
	args: card({
		...base,
		available: false,
		apply: "staged",
		install_kind: "rpm-ostree",
		last_result: {
			ok: true,
			staged: true,
			from: "0.48.0",
			to: "0.49.0",
			finished_unix: NOW - 60,
		},
	}) as never,
};

/** The feed has not moved in weeks. */
export const Stale: Story = {
	args: card({
		...base,
		available: false,
		manifest: {
			...(base.manifest as NonNullable<UpdateStatus["manifest"]>),
			version: "0.48.0",
			stale: true,
		},
	}) as never,
};
