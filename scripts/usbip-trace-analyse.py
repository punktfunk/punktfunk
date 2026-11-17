#!/usr/bin/env python3
"""Walk a `PUNKTFUNK_USBIP_TRACE` capture and find where the two sides stop agreeing.

A USB/IP connection is a framed byte stream whose frame lengths are declared inside the frames, so
one reply that writes a different number of bytes than its header promises shifts everything after
it. The peer then fails at whatever frame happens to land badly, which is never the frame that was
wrong. This walks both directions as PDUs and reports the first frame that does not decode, plus a
per-URB ledger of declared vs. written bytes.

Usage:  usbip-trace-analyse.py /tmp/pad.virtual-DualSense-0
        (reads <prefix>.rx and <prefix>.tx)
"""

import struct
import sys

CMD_SUBMIT, CMD_UNLINK, RET_SUBMIT, RET_UNLINK = 1, 2, 3, 4
NAMES = {1: "CMD_SUBMIT", 2: "CMD_UNLINK", 3: "RET_SUBMIT", 4: "RET_UNLINK"}


def be32(b, o):
    return struct.unpack_from(">I", b, o)[0]


USBIP_VERSION = 0x0111


def skip_handshake(buf, side):
    """Return the offset where URB framing begins.

    A capture starts at `accept()`, so the first bytes are the op-level import handshake, which is
    framed differently (a 2-byte version, not a 4-byte command). Walking it as a PDU decodes as
    garbage and reports a desync at offset 0 — a false positive that would send the reader hunting
    for a framing bug in the one place there is none.
    """
    off = 0
    while off + 4 <= len(buf) and struct.unpack_from(">H", buf, off)[0] == USBIP_VERSION:
        code = struct.unpack_from(">H", buf, off + 2)[0]
        if side == "rx":
            # OP_REQ_IMPORT: status(4) + busid(32); OP_REQ_DEVLIST: status(4).
            off += 40 if code == 0x8003 else 8
        else:
            # OP_REP_IMPORT: status(4) + a 312-byte device record when status == 0.
            status = be32(buf, off + 4)
            off += 8 + (312 if code == 0x0003 and status == 0 else 0)
    return off


def walk(buf, side):
    """Yield decoded PDUs. `side` is 'rx' (kernel -> us) or 'tx' (us -> kernel)."""
    off = skip_handshake(buf, side)
    while off < len(buf):
        if len(buf) - off < 48:
            yield {"off": off, "error": f"truncated header: {len(buf) - off} bytes left"}
            return
        cmd = be32(buf, off)
        pdu = {
            "off": off,
            "cmd": cmd,
            "name": NAMES.get(cmd, f"?{cmd:#x}"),
            "seq": be32(buf, off + 4),
            "dir": be32(buf, off + 12),  # 0 = OUT, 1 = IN
            "ep": be32(buf, off + 16),
        }
        if cmd not in NAMES:
            pdu["error"] = "unknown command — the stream is already desynced at or before here"
            yield pdu
            return

        body = off + 48
        if cmd == CMD_SUBMIT:
            pdu["xfer_len"] = be32(buf, off + 24)
            npkts = be32(buf, off + 32)
            pdu["npkts"] = npkts
            # OUT carries its payload; IN does not.
            payload = pdu["xfer_len"] if pdu["dir"] == 0 else 0
            table = 16 * npkts if npkts not in (0, 0xFFFFFFFF) else 0
            pdu["payload"], pdu["table"] = payload, table
            pdu["setup"] = buf[off + 40 : off + 48].hex()
            off = body + payload + table
        elif cmd == RET_SUBMIT:
            pdu["status"] = be32(buf, off + 20)
            pdu["actual"] = be32(buf, off + 24)
            npkts = be32(buf, off + 32)
            pdu["npkts"] = npkts
            # This is the crux: the kernel reads a payload back only for an IN transfer
            # (`usbip_recv_xbuff` returns early for `usb_pipeout`). Bytes written after an OUT
            # reply's header are never consumed and desync the stream. That holds for isochronous
            # OUT too, where `actual_length` counts bytes *accepted* and no buffer follows — so it
            # must not be read as a payload length here.
            payload = pdu["actual"] if pdu["dir"] == 1 else 0
            table = 16 * npkts if npkts not in (0, 0xFFFFFFFF) else 0
            pdu["payload"], pdu["table"] = payload, table
            off = body + payload + table
        else:  # UNLINK either way: 48 bytes flat
            pdu["payload"], pdu["table"] = 0, 0
            off = body
        pdu["end"] = off
        yield pdu


def main(prefix):
    rx = open(prefix + ".rx", "rb").read()
    tx = open(prefix + ".tx", "rb").read()
    print(f"rx (kernel -> us): {len(rx)} bytes")
    print(f"tx (us -> kernel): {len(tx)} bytes\n")

    submits = {}
    for p in walk(rx, "rx"):
        if "error" in p:
            print(f"!! RX desync at offset {p['off']}: {p['error']}")
            break
        if p["cmd"] == CMD_SUBMIT:
            submits[p["seq"]] = p

    print(f"parsed {len(submits)} CMD_SUBMITs from the kernel")

    bad, replies = [], 0
    for p in walk(tx, "tx"):
        if "error" in p:
            bad.append((p, f"TX desync at offset {p['off']}: {p['error']}"))
            break
        replies += 1
        if p["cmd"] != RET_SUBMIT:
            continue
        req = submits.get(p["seq"])
        # The rule vhci_hcd kills the whole connection over, checked against its own logic — plus
        # the quieter one that kills nothing and breaks everything: an OUT reply's `actual_length`
        # is copied into the URB verbatim and is what `write()` on the device node returns, so an
        # OUT reply claiming 0 against a non-empty request tells every writer it sent nothing.
        # The kernel reads no payload back on OUT whatever the field says, so the framing above
        # never depends on it.
        if req and p["dir"] == 0 and not p["npkts"] and p["actual"] > req["xfer_len"]:
            bad.append((p, f"OUT reply claims actual_length={p['actual']} > the {req['xfer_len']} "
                           f"bytes the kernel sent — a device cannot accept more than it was given"))
        elif req and p["dir"] == 0 and not p["npkts"] and req["xfer_len"] and not p["actual"]:
            bad.append((p, f"OUT reply claims actual_length=0 against a {req['xfer_len']}-byte "
                           f"write — write()/HIDIOCSFEATURE on the device node return 0 and every "
                           f"caller checking '> 0' (winebus does) takes the write as failed"))
        elif req and p["dir"] == 1 and p["actual"] > req["xfer_len"]:
            bad.append((p, f"actual_length {p['actual']} > the {req['xfer_len']} requested "
                           f"(setup {req['setup']}) — usbip_recv_xbuff() calls this a malicious "
                           f"packet: 'recv xbuf, 0' then VDEV_EVENT_ERROR_TCP, which disconnects "
                           f"the device"))
        elif req and req["dir"] != p["dir"]:
            bad.append((p, "direction does not match its CMD_SUBMIT"))

    print(f"parsed {replies} replies from us\n")
    if bad:
        print(f"{len(bad)} BAD frame(s). The first is the bug; the rest is fallout.\n")
        for p, why in bad[:10]:
            d = "IN" if p["dir"] == 1 else "OUT"
            print(f"  offset {p['off']} seq {p['seq']} {d} ep{p['ep']}: {why}")
    else:
        print("Every reply's declared length matches what the kernel will consume.")
        print("If the connection still died, framing is not the cause — look below for a")
        print("CMD_SUBMIT that never got a reply (a missing reply, not a mis-sized one).")

    # An unanswered request is the other way this dies, and it looks identical from dmesg.
    answered = {p["seq"] for p in walk(tx, "tx") if p.get("cmd") in (RET_SUBMIT, RET_UNLINK)}
    missing = [s for s in submits if s not in answered]
    if missing:
        print(f"\n{len(missing)} CMD_SUBMIT(s) never answered: {sorted(missing)[:20]}")
        for s in sorted(missing)[:5]:
            p = submits[s]
            d = "IN" if p["dir"] == 1 else "OUT"
            print(f"   seq {s}: {d} ep{p['ep']} len={p['xfer_len']} setup={p['setup']}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
