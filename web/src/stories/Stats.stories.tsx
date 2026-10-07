import type { Meta, StoryObj } from "@storybook/react-vite";
import { CaptureControlCard } from "@/sections/Diagnostics/Performance/CaptureControl";
import { DetailCard } from "@/sections/Diagnostics/Performance/Detail";
import { RecordingsCard } from "@/sections/Diagnostics/Performance/Recordings";
import { StatsView } from "@/sections/Diagnostics/Performance/view";
import {
	captureDetail,
	captureDetailDriver,
	captureMetas,
	statsStatusIdle,
} from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

// Renders the REAL page layout (StatsView) — the same component index.tsx uses — with the pure
// cards + mock state in its slots, so there's no duplicated composition to drift.
const meta = {
	title: "Pages/Stats",
	component: StatsView,
	parameters: { layout: "padded" },
} satisfies Meta<typeof StatsView>;

export default meta;
type Story = StoryObj<typeof meta>;

// A finished run open in the detail view: recordings table populated and the full graph set
// (latency stack · throughput · loss/FEC) rendered from a deterministic fixture series — no live
// host or capture needed.
export const Recording: Story = {
	args: {
		control: (
			<CaptureControlCard
				status={{ data: statsStatusIdle, ...idle }}
				onStart={noop}
				onStop={noop}
				isStarting={false}
				isStopping={false}
			/>
		),
		live: null,
		recordings: (
			<RecordingsCard
				recordings={{ data: captureMetas, ...idle }}
				selectedId={captureMetas[0]?.id ?? null}
				onSelect={noop}
				onDownload={noop}
				onDelete={noop}
				isDeleting={false}
			/>
		),
		detail: (
			<DetailCard detail={{ data: captureDetail, ...idle }} onClose={noop} />
		),
	},
};

// A Windows capture: the driver's lump against a 120 Hz frame, pool drops, and the sample cap.
export const WindowsDriver: Story = {
	args: {
		control: (
			<CaptureControlCard
				status={{ data: statsStatusIdle, ...idle }}
				onStart={noop}
				onStop={noop}
				isStarting={false}
				isStopping={false}
			/>
		),
		live: null,
		recordings: (
			<RecordingsCard
				recordings={{ data: captureMetas, ...idle }}
				selectedId={captureMetas[1]?.id ?? null}
				onSelect={noop}
				onDownload={noop}
				onDelete={noop}
				isDeleting={false}
			/>
		),
		detail: (
			<DetailCard
				detail={{ data: captureDetailDriver, ...idle }}
				onClose={noop}
			/>
		),
	},
};

export const Empty: Story = {
	args: {
		control: (
			<CaptureControlCard
				status={{ data: statsStatusIdle, ...idle }}
				onStart={noop}
				onStop={noop}
				isStarting={false}
				isStopping={false}
			/>
		),
		live: null,
		recordings: (
			<RecordingsCard
				recordings={{ data: [], ...idle }}
				selectedId={null}
				onSelect={noop}
				onDownload={noop}
				onDelete={noop}
				isDeleting={false}
			/>
		),
		detail: null,
	},
};
