# 0002 — Encode backend resolution

Status: accepted, 2026-10-20.

## Context

Before and during every session the host asks `pf-encode` which backend opens and what it can
do: the codecs to advertise, 10-bit, 4:4:4, the zero-copy plane, the cursor blend, the capture
modifiers. Sixteen free functions answered, each reading the Encoder row and the selected GPU
again and caching its probe under its own key. A key that missed an input answered for the
wrong backend (the Vulkan row, then the Encoder row, each a fix). A probe that found nothing was
kept for the process. The open read the config once more, so the backend it opened and the one
the handshake advertised could differ.

## Decision

1. **Resolve once.** `pf_encode::resolve()` returns an `Arc<Resolved>`. Every capability
   question is a method on it, and `open_video(&Resolved, OpenParams)` opens the backend it
   names. A host entry point (a handshake, a `/serverinfo` poll, the management host info, a
   diagnostic command) resolves once and passes the value down.
2. **The key is the whole input.** `ResolveInputs` holds the Encoder row, the selected GPU's key,
   the Vulkan encoding row, NVIDIA's device nodes and the build's features. There is one
   `Resolved` per key for the process, and a change to any of them resolves afresh. An input
   that changes an answer joins the key; no answer keeps a cache of its own.
3. **Probes are lazy and kept.** Each probe fills a cell the first time a method needs it;
   building a `Resolved` probes nothing. An explicit Encoder row never runs the `auto` probe. A
   probe that could not ask (a codec set that found nothing, a driver that listed no codecs, no
   VAAPI display, no Vulkan instance) leaves its cell empty and is not asked again for 30
   seconds: asks inside that window answer "unanswered" without probing, so asks made together
   agree and a box that never answers costs one probe per window. The first ask after it
   probes again; a codec probe warns about the miss once. The 4:4:4 and 10-bit verdicts read
   off such a cell are kept only once it holds an answer.
4. **Methods read the value.** A method reads the inputs and the cells, never the config or the
   GPU again, so the resolution table runs over hand-filled cells without a GPU.
5. **Edges.** Capture never asks encode: the host resolves and hands pf-capture values
   (`ZeroCopyPolicy`, the output format). Encode never reads capture.

## Consequences

- A new capability is a method on `Resolved`; a new probe is a cell.
- The resolution table in `pf-encode/src/resolve*` names the fix each row pins.
- A session keeps the `Resolved` its handshake negotiated: every plan, capture and encoder open
  inside it, rebuilds included, reads that value, so an Encoder-row change applies at the next
  session.
- Rejected: a capabilities struct filled at once (every poll would run every probe), and the
  per-function caches with wider keys (the drift this replaces).

Evidence: `design/pf-encode-overhaul.md` §4.2 and `design/pf-encode-overhaul/04-design-note.md`
in the planning repository.
