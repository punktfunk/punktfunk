// The console's numeric field IS @unom/ui's form input-number — the Input with a number's rules
// layered on: it keeps a local draft while you type (so the field can be EMPTY on the way to a new
// value), commits only a finite in-range number, and clamps to `min`/`max` on blur.
//
// That is not cosmetic. A hand-rolled shape —
//
//     <Input type="number" min={1} max={600}
//            onChange={(e) => set({ timeout_s: Number(e.target.value) || 30 })} />
//
// — has two defects the shared component doesn't: clearing the field snaps it to the fallback
// instead of letting you retype, and `min`/`max` are decoration (an `<input>`'s range is only
// enforced by form validation, which a controlled field like this never runs) — so an
// out-of-range value goes straight through.
//
// `onChange` hands you a `number`, not an event.
export { InputNumber } from "@unom/ui/form/input-number";
