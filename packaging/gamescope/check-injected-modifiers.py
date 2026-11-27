#!/usr/bin/env python3
"""Check that keys injected over gamescope's EIS socket carry their modifiers to X11.

Presses Shift, Ctrl and Caps Lock the way a remote client's keys arrive, and reads the modifier
state the X server stamps on every key event. Exits 1 when a modifier does not arrive.

Needs libei, libX11 and an X11 window with the keyboard in that session: Xwayland only
hears keys while one of its windows has it.

Usage: check-injected-modifiers.py <eis-socket> <x-display>
  e.g. check-injected-modifiers.py "$XDG_RUNTIME_DIR/gamescope-0-ei" :0
"""

import ctypes
import select
import sys
import time
from ctypes import c_bool, c_char_p, c_int, c_uint, c_uint32, c_uint64, c_void_p

KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_CAPSLOCK = 29, 42, 58
SHIFT_MASK, LOCK_MASK, CONTROL_MASK = 0x1, 0x2, 0x4
# libei.h: ei_event_type, ei_device_capability.
EI_EVENT_SEAT_ADDED, EI_EVENT_DEVICE_RESUMED = 3, 8
EI_DEVICE_CAP_KEYBOARD = 1 << 2
XKB_USE_CORE_KBD = 0x0100


class XkbState(ctypes.Structure):
    """XkbStateRec from <X11/extensions/XKBstr.h>."""

    _fields_ = [
        ("group", ctypes.c_ubyte),
        ("locked_group", ctypes.c_ubyte),
        ("base_group", ctypes.c_ushort),
        ("latched_group", ctypes.c_ushort),
        ("mods", ctypes.c_ubyte),
        ("base_mods", ctypes.c_ubyte),
        ("latched_mods", ctypes.c_ubyte),
        ("locked_mods", ctypes.c_ubyte),
        ("compat_state", ctypes.c_ubyte),
        ("grab_mods", ctypes.c_ubyte),
        ("compat_grab_mods", ctypes.c_ubyte),
        ("lookup_mods", ctypes.c_ubyte),
        ("compat_lookup_mods", ctypes.c_ubyte),
        ("ptr_buttons", ctypes.c_ushort),
    ]


def load_libei():
    ei = ctypes.CDLL("libei.so.1")
    for name, restype, argtypes in (
        ("ei_new_sender", c_void_p, [c_void_p]),
        ("ei_configure_name", None, [c_void_p, c_char_p]),
        ("ei_setup_backend_socket", c_int, [c_void_p, c_char_p]),
        ("ei_get_fd", c_int, [c_void_p]),
        ("ei_dispatch", None, [c_void_p]),
        ("ei_get_event", c_void_p, [c_void_p]),
        ("ei_event_get_type", c_int, [c_void_p]),
        ("ei_event_get_seat", c_void_p, [c_void_p]),
        ("ei_event_get_device", c_void_p, [c_void_p]),
        ("ei_event_unref", c_void_p, [c_void_p]),
        ("ei_device_ref", c_void_p, [c_void_p]),
        ("ei_device_start_emulating", None, [c_void_p, c_uint32]),
        ("ei_device_keyboard_key", None, [c_void_p, c_uint32, c_bool]),
        ("ei_device_frame", None, [c_void_p, c_uint64]),
        ("ei_now", c_uint64, [c_void_p]),
    ):
        fn = getattr(ei, name)
        fn.restype, fn.argtypes = restype, argtypes
    return ei


class Keyboard:
    """A sender on gamescope's EIS socket with its keyboard device."""

    def __init__(self, socket):
        self.lib = load_libei()
        self.ei = self.lib.ei_new_sender(None)
        self.lib.ei_configure_name(self.ei, b"punktfunk modifier check")
        err = self.lib.ei_setup_backend_socket(self.ei, socket.encode())
        if err:
            sys.exit(f"connect {socket}: errno {-err}")
        self.device = None
        deadline = time.monotonic() + 5
        while self.device is None:
            if time.monotonic() > deadline:
                sys.exit(f"{socket} offered no keyboard within 5 s")
            self.pump(0.2)
        self.lib.ei_device_start_emulating(self.device, 1)

    def pump(self, wait):
        select.select([self.lib.ei_get_fd(self.ei)], [], [], wait)
        self.lib.ei_dispatch(self.ei)
        while event := self.lib.ei_get_event(self.ei):
            kind = self.lib.ei_event_get_type(event)
            if kind == EI_EVENT_SEAT_ADDED:
                # Variadic and NULL-terminated, so it takes no argtypes.
                self.lib.ei_seat_bind_capabilities(
                    c_void_p(self.lib.ei_event_get_seat(event)),
                    c_int(EI_DEVICE_CAP_KEYBOARD),
                    c_void_p(None),
                )
            elif kind == EI_EVENT_DEVICE_RESUMED:
                self.device = self.lib.ei_device_ref(self.lib.ei_event_get_device(event))
            self.lib.ei_event_unref(event)

    def key(self, code, down):
        self.lib.ei_device_keyboard_key(self.device, code, down)
        self.lib.ei_device_frame(self.device, self.lib.ei_now(self.ei))
        # Long enough for gamescope to forward the key and Xwayland to read it.
        self.pump(0.15)

    def tap(self, code):
        self.key(code, True)
        self.key(code, False)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    socket, display = sys.argv[1], sys.argv[2]

    x11 = ctypes.CDLL("libX11.so.6")
    x11.XOpenDisplay.restype = c_void_p
    x11.XOpenDisplay.argtypes = [c_char_p]
    x11.XkbGetState.argtypes = [c_void_p, c_uint, ctypes.POINTER(XkbState)]
    x11.XQueryKeymap.argtypes = [c_void_p, ctypes.c_ubyte * 32]
    dpy = x11.XOpenDisplay(display.encode())
    if not dpy:
        sys.exit(f"open X display {display}")

    def state():
        rec = XkbState()
        if x11.XkbGetState(dpy, XKB_USE_CORE_KBD, ctypes.byref(rec)):
            sys.exit(f"read the XKB state of {display}")
        return rec

    def is_down(evdev):
        keys = (ctypes.c_ubyte * 32)()
        x11.XQueryKeymap(dpy, keys)
        keycode = evdev + 8
        return bool(keys[keycode // 8] & (1 << (keycode % 8)))

    keyboard = Keyboard(socket)
    lock_before = state().locked_mods & LOCK_MASK

    keyboard.key(KEY_LEFTSHIFT, True)
    shift_held = state().mods & SHIFT_MASK
    arrived = is_down(KEY_LEFTSHIFT)
    keyboard.key(KEY_LEFTSHIFT, False)
    if not arrived:
        sys.exit(f"no key reached {display}: none of its windows has the keyboard")
    shift_released = state().mods & SHIFT_MASK

    keyboard.key(KEY_LEFTCTRL, True)
    ctrl_held = state().mods & CONTROL_MASK
    keyboard.key(KEY_LEFTCTRL, False)

    keyboard.tap(KEY_CAPSLOCK)
    lock_toggled = state().locked_mods & LOCK_MASK
    keyboard.tap(KEY_CAPSLOCK)
    lock_restored = state().locked_mods & LOCK_MASK

    checks = (
        ("Shift held sets Shift", bool(shift_held)),
        ("Shift released clears Shift", not shift_released),
        ("Ctrl held sets Control", bool(ctrl_held)),
        ("Caps Lock toggles Lock", lock_toggled != lock_before),
        ("a second Caps Lock restores Lock", lock_restored == lock_before),
    )
    for what, ok in checks:
        print(f"{'ok  ' if ok else 'FAIL'} {what}")
    if not all(ok for _, ok in checks):
        sys.exit("FAIL: X11 does not see the modifiers of injected keys")
    print("PASS")


if __name__ == "__main__":
    main()
