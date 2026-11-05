// The console's Checkbox IS @unom/ui's radix checkbox: shadcn tokens (border-input,
// data-[state=checked]:bg-primary), an animated tick and the shared toggle sound.
// It takes `checked`/`onCheckedChange` (radix), not `onChange`. A raw
// `<input type="checkbox">` in its place loses the brand.
export { Checkbox } from "@unom/ui/form/checkbox";
