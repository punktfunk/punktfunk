# loss-harness

A **FEC loss-resilience sweep** for [`punktfunk-core`](../../crates/core/punktfunk-core/README.md). It
drives access units through the in-process loopback at increasing packet-loss rates — for **both** FEC
schemes (GF(2⁸) and GF(2¹⁶)) — and reports how many frames survive.

It's a pure-software stand-in for `tc netem`: no network, no root, runs anywhere `punktfunk-core`
builds. Use it to sanity-check the FEC before reaching for the real `punktfunk/1` harness (which adds
`tc netem` jitter/reorder on the UDP path).

```sh
cargo run -p loss-harness        # from the repo root
```

Part of the measurement tooling (punktfunk-planning: `implementation-plan.md` §10), alongside
[`latency-probe`](../latency-probe/README.md).
