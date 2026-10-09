import type { FC } from "react";
import { apiFetch } from "@/api/fetcher";
import type { ActionInfo } from "@/api/gen/model";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { m } from "@/paraglide/messages";

/** Localized titles for the KNOWN action ids; unknown ids fall back to the server's title —
 * the contract that lets future host actions appear with no console release. */
export const actionTitle = (a: ActionInfo): string => {
	switch (a.id) {
		case "power.sleep":
			return m.host_power_sleep();
		case "power.reboot":
			return m.host_power_reboot();
		case "power.shutdown":
			return m.host_power_shutdown();
		case "host.restart":
			return m.host_power_restart_service();
		default:
			return a.title;
	}
};

/** Runs a host action once the console password is re-entered; the BFF verifies it. */
export const ConfirmDialog: FC<{
	action: ActionInfo;
	onClose: () => void;
	onAccepted: (action: ActionInfo) => void;
}> = ({ action, onClose, onAccepted }) => (
	<PasswordConfirmDialog
		open
		id="host-power-password"
		title={m.host_power_confirm_title({ action: actionTitle(action) })}
		body={m.host_power_confirm_body()}
		submitLabel={actionTitle(action)}
		busyLabel={m.host_power_working()}
		destructive={action.danger}
		onSubmit={async (password) => {
			await apiFetch(`/api/v1/actions/${encodeURIComponent(action.id)}`, {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ password }),
			});
			onAccepted(action);
		}}
		onClose={onClose}
	/>
);
