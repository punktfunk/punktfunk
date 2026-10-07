// One session's live pad feed: `GET /api/v1/session/{id}/pads`, folded for the screen.
//
// Opened only while this page is mounted. The host publishes nothing with nobody attached,
// so a console anywhere else costs it nothing — closing the stream is what turns the tap off.
//
// Two rates, on purpose. The drawing is repainted once per animation frame, because a stick
// sweep arrives at up to ~250 Hz and a screen is not that fast. The log is derived from EVERY
// frame as it lands, so a tap between two paints is still recorded.

import { useEffect, useRef, useState } from "react";
import type { PadFrame } from "@/api/gen/model/padFrame";
import {
	anchorOf,
	appendLog,
	type PadAnchor,
	type PadLogLine,
	padEvents,
} from "./pads";

export interface PadFeedState {
	/** One frame per pad the host holds, by wire index. */
	pads: PadFrame[];
	log: PadLogLine[];
	connected: boolean;
	/** Set when the stream ends badly; the page shows it instead of a frozen drawing. */
	failed: boolean;
}

const EMPTY: PadFeedState = {
	pads: [],
	log: [],
	connected: false,
	failed: false,
};

export function usePadFeed(
	sessionId: number | undefined,
	paused: boolean,
): PadFeedState {
	const [state, setState] = useState<PadFeedState>(EMPTY);
	// Read inside the frame handler, so pausing does not tear down the stream.
	const pausedRef = useRef(paused);
	pausedRef.current = paused;

	useEffect(() => {
		if (sessionId === undefined) {
			setState(EMPTY);
			return;
		}
		const live = new Map<number, PadFrame>();
		const anchors = new Map<number, PadAnchor>();
		let log: PadLogLine[] = [];
		let seq = 0;
		let connected = false;
		let painting = 0;

		const paint = () => {
			painting = 0;
			setState({
				pads: [...live.values()].sort((a, b) => a.pad - b.pad),
				log,
				connected,
				failed: false,
			});
		};
		const schedule = () => {
			if (!painting) painting = requestAnimationFrame(paint);
		};

		const es = new EventSource(`/api/v1/session/${sessionId}/pads`);
		es.addEventListener("open", () => {
			connected = true;
			schedule();
		});
		es.addEventListener("pad.state", (e) => {
			let frame: PadFrame;
			try {
				frame = JSON.parse((e as MessageEvent).data) as PadFrame;
			} catch {
				return;
			}
			if (!pausedRef.current) {
				const { texts, anchor } = padEvents(anchors.get(frame.pad), frame);
				anchors.set(frame.pad, anchor);
				log = appendLog(log, frame.pad, frame.ts_ms, texts, seq);
				seq += texts.length;
			} else if (!anchors.has(frame.pad)) {
				anchors.set(frame.pad, anchorOf(frame));
			}
			if (frame.present) live.set(frame.pad, frame);
			else {
				live.delete(frame.pad);
				anchors.delete(frame.pad);
			}
			schedule();
		});
		// EventSource reconnects by itself; a stream the host refused (404, cap) never opens,
		// and that is the one the page has to name.
		es.addEventListener("error", () => {
			if (!connected) {
				setState((s) => ({ ...s, connected: false, failed: true }));
			}
		});

		return () => {
			es.close();
			if (painting) cancelAnimationFrame(painting);
			setState(EMPTY);
		};
	}, [sessionId]);

	return state;
}
