"""The Punktfunk Decky backend, split by concern. ``main.py`` holds only the bridge class.

Decky appends ``<plugin>/py_modules`` to the END of ``sys.path`` and loads ``main.py`` under the
module name ``_``, so the plugin root is not importable: no module here imports ``main``. One
uniquely named package keeps a generic module name from being shadowed by the loader's own
libraries. The plugins dir is root-owned, so nothing may rely on writing ``__pycache__`` here.

* ``client`` — which client is installed, the session env, the headless CLI and its verbs.
* ``update`` — the plugin's own manifest and the client's update route, behind one cache.
* ``steam_input`` — the native-touch controller layout written into Steam's config.
* ``art`` — shortcut artwork: the plugin's own, a game's own art and icon.
* ``session`` — the running stream: control socket, end game, force stop.

A call into another module goes through the module (``client._run_cli``), so one patch in a test
reaches every caller. ``scripts/test-backend.py`` pins the loader shape.
"""
