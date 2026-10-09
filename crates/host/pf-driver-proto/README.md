# pf-driver-proto

The host ↔ `pf-vdisplay` binary contract — control IOCTLs and the encode transport — defined once.
`src/lib.rs`'s module docs describe both planes; this file exists only to explain the crate's shape.

It is a path dependency of **two** build graphs: the host workspace
([`crates/host/punktfunk-host`](../punktfunk-host)) and the out-of-workspace driver workspace
([`packaging/windows/drivers/`](../../../packaging/windows/drivers)). It must resolve identically from
either, which is why it is `no_std` (+ alloc), carries no `*.workspace = true` inheritance, and
passes GUID and LUID as plain integers each side converts to its own OS types.

The `const` size and offset asserts are the point: a one-sided edit to a wire struct becomes a
compile error rather than a silently corrupt frame or IOCTL.
