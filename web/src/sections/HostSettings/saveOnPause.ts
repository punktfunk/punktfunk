// A value that saves itself once it has been still for `ms`, and at once on `flush`.
//
// `host` is what the host holds. A change back to it sends nothing. `follow` says whether the
// field may show a new host value: not while an edit waits, or the answer to the last save
// would overwrite what the player is typing.
export function saveOnPause<T>(ms: number, host: T) {
	let edit: { value: T; send: (value: T) => void } | null = null;
	let timer: ReturnType<typeof setTimeout> | undefined;

	// Clears the edit before it sends: a blur that follows the timer finds nothing to send.
	const flush = () => {
		clearTimeout(timer);
		const e = edit;
		edit = null;
		if (e && e.value !== host) e.send(e.value);
	};

	return {
		change(value: T, send: (value: T) => void) {
			clearTimeout(timer);
			edit = { value, send };
			timer = setTimeout(flush, ms);
		},
		flush,
		follow(value: T) {
			host = value;
			return edit === null;
		},
	};
}
