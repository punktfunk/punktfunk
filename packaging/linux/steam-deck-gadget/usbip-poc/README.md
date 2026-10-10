# usbip Deck PoC — the shippable virtual Deck for non-SteamOS Linux hosts

Presents a real 3-interface USB Steam Deck (`28DE:1205`, controller on **interface 2**) over the
usbip protocol via the `usbip` crate, so `vhci_hcd` can attach it **locally** — the Secure-Boot-clean,
universal alternative to `dummy_hcd`+`raw_gadget` (which Bazzite/Fedora don't ship). Reuses the exact
captured descriptors + feature contract from `crates/host/punktfunk-host/src/inject/linux/steam_gadget.rs`.

**Validated live on Bazzite (2026-06-29):** `vhci_hcd` enumerates it, `hid-steam` binds it + reads the
serial + makes the `Steam Deck` evdevs, stable (1 connect / 0 disconnect), and **Steam promotes it**
(`Interface: 2 … reserving XInput slot 1`, X-Box pad emitted) — identical to the gadget on SteamOS.

```sh
cargo build --release           # glibc; needs GLIBC_2.34 (Bazzite has 2.42), libusb present/vendored
# on the host (root for the last two):
sudo modprobe vhci_hcd
./usbip-deck-poc pressa &        # the usbip server (runs as a normal user, TCP 127.0.0.1:3240)
sudo usbip attach -r 127.0.0.1 -b 0-0-0
```

See punktfunk-planning: `steam-deck-passthrough-plan.md` for the production build plan (vendor-trim the crate to
drop the `rusb`/libusb dep; in-process `vhci_hcd` attach to avoid the `usbip` CLI; transport-select
`raw_gadget`→`usbip`→UHID). `usbip` crate API: a custom `UsbInterfaceHandler` —
`get_class_specific_descriptor()` = the 9-byte HID descriptor; `handle_urb()` dispatches EP0
`GET_DESCRIPTOR`(report) / HID `GET_REPORT`(=`feature_reply`) / `SET_REPORT`, and returns the 64-byte
state report on the interrupt-IN endpoint.
