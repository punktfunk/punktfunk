/** The part of a key event `isImeEvent` reads. React's `KeyboardEvent` has both. */
type KeyEventLike = { nativeEvent: { isComposing: boolean }; keyCode: number };

/**
 * True while an input method is composing. Its Enter picks a candidate and is not the player's
 * Enter. Safari ends the composition before that keydown arrives, so `isComposing` is already
 * false there and only `keyCode` 229 still says so.
 */
export const isImeEvent = (e: KeyEventLike): boolean =>
	e.nativeEvent.isComposing || e.keyCode === 229;
