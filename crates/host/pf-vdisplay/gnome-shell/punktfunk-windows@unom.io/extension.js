// Serves the shell's window list to the punktfunk host over D-Bus. GNOME gives Wayland clients no
// window list of its own, and the host needs one to tell when a launched game's window is up.
// Titles stay in the shell: the host matches on pid and class alone.

import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.unom.Punktfunk.Shell';
const OBJECT_PATH = '/io/unom/Punktfunk/Shell';
const IFACE = `
<node>
  <interface name="io.unom.Punktfunk.Shell">
    <method name="ListWindows">
      <arg type="a(usbbs)" direction="out" name="windows"/>
    </method>
  </interface>
</node>`;

export default class PunktfunkWindows extends Extension {
    enable() {
        this._object = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._object.export(Gio.DBus.session, OBJECT_PATH);
        this._owner = Gio.bus_own_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameOwnerFlags.NONE, null, null, null);
    }

    disable() {
        if (this._owner)
            Gio.bus_unown_name(this._owner);
        this._owner = 0;
        this._object?.unexport();
        this._object = null;
    }

    // (pid, class, focused, fullscreen, id) for each normal window that is not minimized.
    ListWindows() {
        return global.get_window_actors()
            .map(actor => actor.get_meta_window())
            .filter(w => w && w.get_window_type() === Meta.WindowType.NORMAL && !w.minimized)
            .map(w => [
                Math.max(w.get_pid(), 0),
                w.get_wm_class() ?? '',
                w.has_focus(),
                w.is_fullscreen(),
                String(w.get_stable_sequence()),
            ]);
    }
}
