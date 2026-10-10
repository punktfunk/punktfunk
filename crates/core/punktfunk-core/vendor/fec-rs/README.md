# fec-rs

[![CI](https://github.com/hgaiser/fec-rs/workflows/CI/badge.svg)](https://github.com/hgaiser/fec-rs/actions)
[![Crates.io](https://img.shields.io/crates/v/fec-rs.svg)](https://crates.io/crates/fec-rs)
[![Documentation](https://docs.rs/fec-rs/badge.svg)](https://docs.rs/fec-rs)

A pure Rust Reed-Solomon erasure coding library with runtime SIMD acceleration.

## Features

- **Pure Rust** — No C/C++ dependencies or FFI. Everything is implemented in safe Rust
  (with targeted `unsafe` for SIMD intrinsics).
- **Runtime SIMD detection** — Automatically uses the fastest available instruction set
  via `std::is_x86_feature_detected!`. A single binary works on all x86_64 systems.
- **GF(2^8)** — Operates over the Galois field GF(2^8) with generating polynomial 29 (0x1D),
  compatible with the Moonlight streaming protocol.
- **Shard-by-shard encoding** — Incremental encoding via `ShardByShard` for streaming use cases.
- **Reconstruction** — Reconstruct missing data and/or parity shards from any sufficient subset.

## SIMD Acceleration

On x86_64, the library automatically detects CPU features at runtime and uses
the best available instruction set:

- **GFNI + AVX2** — Single-instruction GF multiply on 32 bytes (Intel Alder Lake+, AMD Zen 4+)
- **AVX2** — VPSHUFB split-table nibble lookup on 32 bytes
- **GFNI + SSE** — Single-instruction GF multiply on 16 bytes
- **SSSE3** — VPSHUFB split-table nibble lookup on 16 bytes
- **Scalar** — Lookup table fallback

## Parallel Encoding

Enable the `parallel` feature for optional rayon-based parallel encoding:

```toml
fec-rs = { version = "0.1", features = ["parallel"] }
```

When enabled, large encode workloads automatically distribute parity shard
computation across threads. Small workloads use the sequential path to avoid
overhead.

## Usage

```rust
use fec_rs::ReedSolomon;

let rs = ReedSolomon::new(4, 2).unwrap();

let mut shards: Vec<Vec<u8>> = vec![
    vec![0, 1, 2, 3],
    vec![4, 5, 6, 7],
    vec![8, 9, 10, 11],
    vec![12, 13, 14, 15],
    vec![0, 0, 0, 0], // parity shard 1
    vec![0, 0, 0, 0], // parity shard 2
];

// Encode parity
rs.encode(&mut shards).unwrap();

// Verify
assert!(rs.verify(&shards).unwrap());

// Simulate loss of shard 0
let mut recovery: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
recovery[0] = None;

// Reconstruct
rs.reconstruct(&mut recovery).unwrap();
```

License: BSD-2-Clause
