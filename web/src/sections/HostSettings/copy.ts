// The words for each host setting, keyed by the registry id the host sends.
//
// The host owns which settings exist; this file owns how they read. A setting the host knows and
// this console does not falls back to the host's English `title` and shows no hint, so a newer
// host never renders an unlabelled row.
import type { SettingGroup } from "@/api/gen/model";
import { m } from "@/paraglide/messages";

type Copy = {
	label: () => string;
	hint?: () => string;
	/** Enum option labels, keyed by the canonical value. */
	options?: Record<string, () => string>;
};

/** The three states of a setting whose default the host decides. */
const TRI: Record<string, () => string> = {
	auto: m.host_settings_auto,
	on: m.common_on,
	off: m.common_off,
};

export const SETTING_COPY: Record<string, Copy> = {
	gamestream: {
		label: m.setting_gamestream,
		hint: m.setting_gamestream_hint,
	},
	webtransport: {
		label: m.setting_webtransport,
		hint: m.setting_webtransport_hint,
	},
	clipboard: {
		label: m.setting_clipboard,
		hint: m.setting_clipboard_hint,
		options: {
			off: m.common_off,
			text: m.setting_clipboard_text,
			files: m.setting_clipboard_files,
		},
	},
	host_name: {
		label: m.setting_host_name,
		hint: m.setting_host_name_hint,
	},
	ten_bit: {
		label: m.setting_ten_bit,
		hint: m.setting_ten_bit_hint,
	},
	chroma_444: {
		label: m.setting_chroma_444,
		hint: m.setting_chroma_444_hint,
	},
	max_fps: {
		label: m.setting_max_fps,
		hint: m.setting_max_fps_hint,
	},
	audio_output_mode: {
		label: m.setting_audio_output_mode,
		options: {
			client_only: m.setting_audio_output_mode_client_only,
			host_and_client: m.setting_audio_output_mode_host_and_client,
			follow_default: m.setting_audio_output_mode_follow_default,
		},
	},
	audio_voice_chat: {
		label: m.setting_audio_voice_chat,
		hint: m.setting_audio_voice_chat_hint,
		options: {
			stream: m.setting_audio_voice_chat_stream,
			host: m.setting_audio_voice_chat_host,
		},
	},
	audio_voice_apps: {
		label: m.setting_audio_voice_apps,
		hint: m.setting_audio_voice_apps_hint,
	},
	gamestream_encrypt: {
		label: m.setting_gamestream_encrypt,
		hint: m.setting_gamestream_encrypt_hint,
		options: {
			supported: m.setting_gamestream_encrypt_supported,
			video: m.setting_gamestream_encrypt_video,
			off: m.common_off,
			required: m.setting_gamestream_encrypt_required,
		},
	},
	gamestream_adapt: {
		label: m.setting_gamestream_adapt,
		hint: m.setting_gamestream_adapt_hint,
	},
	chacha20: {
		label: m.setting_chacha20,
		hint: m.setting_chacha20_hint,
	},
	webtransport_origins: {
		label: m.setting_webtransport_origins,
		hint: m.setting_webtransport_origins_hint,
	},
	encoder: {
		label: m.setting_encoder,
		hint: m.setting_encoder_hint,
		options: {
			auto: m.host_settings_auto,
			nvenc: m.setting_encoder_nvenc,
			vaapi: m.setting_encoder_vaapi,
			vulkan: m.setting_encoder_vulkan,
			pyrowave: m.setting_encoder_pyrowave,
			software: m.setting_encoder_software,
			amf: m.setting_encoder_amf,
			qsv: m.setting_encoder_qsv,
			mf: m.setting_encoder_mf,
		},
	},
	portal_cursor_mode: {
		label: m.setting_portal_cursor_mode,
		hint: m.setting_portal_cursor_mode_hint,
		options: {
			auto: m.host_settings_auto,
			embedded: m.setting_portal_cursor_mode_embedded,
			metadata: m.setting_portal_cursor_mode_metadata,
			hidden: m.setting_portal_cursor_mode_hidden,
		},
	},
	vulkan_encode: {
		label: m.setting_vulkan_encode,
		hint: m.setting_vulkan_encode_hint,
	},
	direct_capture: {
		label: m.setting_direct_capture,
		hint: m.setting_direct_capture_hint,
	},
	lazy_capture: {
		label: m.setting_lazy_capture,
		hint: m.setting_lazy_capture_hint,
	},
	kwin_paced: {
		label: m.setting_kwin_paced,
		hint: m.setting_kwin_paced_hint,
	},
	pyrowave_bpp: {
		label: m.setting_pyrowave_bpp,
		hint: m.setting_pyrowave_bpp_hint,
	},
	pyrowave_max_mbps: {
		label: m.setting_pyrowave_max_mbps,
		hint: m.setting_pyrowave_max_mbps_hint,
	},
	audio_quality: {
		label: m.setting_audio_quality,
		options: {
			low: m.setting_audio_quality_low,
			standard: m.setting_audio_quality_standard,
			high: m.setting_audio_quality_high,
		},
	},
	audio_hires: {
		label: m.setting_audio_hires,
		hint: m.setting_audio_hires_hint,
	},
	pad_audio: {
		label: m.setting_pad_audio,
		hint: m.setting_pad_audio_hint,
	},
	audio_redundancy: {
		label: m.setting_audio_redundancy,
		hint: m.setting_audio_redundancy_hint,
		options: TRI,
	},
	gamepad: {
		label: m.setting_gamepad,
		hint: m.setting_gamepad_hint,
		options: {
			auto: m.host_settings_auto,
			xbox360: m.setting_gamepad_xbox360,
			xboxone: m.setting_gamepad_xboxone,
			xboxelite: m.setting_gamepad_xboxelite,
			dualsense: m.setting_gamepad_dualsense,
			dualsenseedge: m.setting_gamepad_dualsenseedge,
			dualshock4: m.setting_gamepad_dualshock4,
			steamdeck: m.setting_gamepad_steamdeck,
			steamcontroller: m.setting_gamepad_steamcontroller,
			steamcontroller2: m.setting_gamepad_steamcontroller2,
			switchpro: m.setting_gamepad_switchpro,
		},
	},
	pen: {
		label: m.setting_pen,
		hint: m.setting_pen_hint,
	},
	steam_gadget: {
		label: m.setting_steam_gadget,
		hint: m.setting_steam_gadget_hint,
		options: TRI,
	},
	dualsense_usbip: {
		label: m.setting_dualsense_usbip,
		hint: m.setting_dualsense_usbip_hint,
	},
	gamescope_attach: {
		label: m.setting_gamescope_attach,
		hint: m.setting_gamescope_attach_hint,
	},
	gamescope_hdr: {
		label: m.setting_gamescope_hdr,
		hint: m.setting_gamescope_hdr_hint,
	},
	gamescope_managed: {
		label: m.setting_gamescope_managed,
		hint: m.setting_gamescope_managed_hint,
	},
	gamescope_vrr: {
		label: m.setting_gamescope_vrr,
		hint: m.setting_gamescope_vrr_hint,
	},
	gamescope_sdr_nits: {
		label: m.setting_gamescope_sdr_nits,
		hint: m.setting_gamescope_sdr_nits_hint,
	},
	gamescope_refresh_rates: {
		label: m.setting_gamescope_refresh_rates,
		hint: m.setting_gamescope_refresh_rates_hint,
	},
	gamescope_steam: {
		label: m.setting_gamescope_steam,
		hint: m.setting_gamescope_steam_hint,
	},
	gamescope_splash: {
		label: m.setting_gamescope_splash,
		hint: m.setting_gamescope_splash_hint,
	},
	gamescope_isolate: {
		label: m.setting_gamescope_isolate,
		hint: m.setting_gamescope_isolate_hint,
	},
	gamescope_grab_cursor: {
		label: m.setting_gamescope_grab_cursor,
		hint: m.setting_gamescope_grab_cursor_hint,
	},
	gamescope_bind: {
		label: m.setting_gamescope_bind,
		hint: m.setting_gamescope_bind_hint,
		options: TRI,
	},
	session_watch: {
		label: m.setting_session_watch,
		hint: m.setting_session_watch_hint,
		options: TRI,
	},
	mdns: {
		label: m.setting_mdns,
		hint: m.setting_mdns_hint,
	},
	idle_timeout_ms: {
		label: m.setting_idle_timeout_ms,
		hint: m.setting_idle_timeout_ms_hint,
	},
	update_check: {
		label: m.setting_update_check,
		hint: m.setting_update_check_hint,
	},
	update_apply: {
		label: m.setting_update_apply,
		hint: m.setting_update_apply_hint,
	},
};

export const GROUP_LABEL: Record<SettingGroup, () => string> = {
	streaming: m.settings_group_streaming,
	video: m.settings_group_video,
	audio: m.settings_group_audio,
	input: m.settings_group_input,
	network: m.settings_group_network,
	game_mode: m.settings_group_game_mode,
	session: m.settings_group_session,
	system: m.settings_group_system,
};
