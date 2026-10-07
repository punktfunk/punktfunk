import type { Meta, StoryObj } from "@storybook/react-vite";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import { HostStrip } from "@/sections/Host/Strip";
import type { Update } from "@/sections/Host/UpdateCard";
import { compositors, hostInfo } from "./lib/fixtures";
import { Routed } from "./lib/routed";

const noop = () => {};
const current: Update = {
	state: {
		data: {
			apply: "full",
			available: false,
			channel: "stable",
			channel_hint: "",
			check_disabled: false,
			current_version: hostInfo.app_version,
			install_kind: "rpm",
			not_published: false,
			manifest: { version: hostInfo.app_version, stale: false },
		},
		isLoading: false,
		error: null,
	},
	onCheck: noop,
	checkBusy: false,
	applying: null,
	onApplied: noop,
	onGiveUp: noop,
} as unknown as Update;

/** The top of Host: who it is, how current, how a device reaches it; the facts fold. */
const meta = {
	title: "Pages/Host",
	component: HostStrip,
	decorators: [
		(Story) => (
			<Routed>
				<Story />
			</Routed>
		),
	],
	args: { host: hostInfo, compositors, update: current },
} satisfies Meta<typeof HostStrip>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

/** A Windows host wears the Windows mark and lists no compositors. */
export const WindowsHost: Story = {
	args: {
		host: { ...hostInfo, os: "windows", os_name: "Windows" } as HostInfo,
		compositors: [],
	},
};

/** A gaming distro wears its OWN mark, not its family's: `cachyos` resolves before `arch`. */
export const CachyOsHost: Story = {
	args: {
		host: {
			...hostInfo,
			os: "linux/arch/cachyos",
			os_name: "CachyOS Linux",
		} as HostInfo,
	},
};

/** An unrecognized distro walks up to its family mark, here all the way to generic Tux. */
export const UnknownDistro: Story = {
	args: {
		host: {
			...hostInfo,
			os: "linux/frontier/chimera",
			os_name: "Chimera Linux",
		} as HostInfo,
	},
};
