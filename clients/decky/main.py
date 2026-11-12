"""Punktfunk Decky plugin: the backend bridge the Gaming-Mode panel calls (``src/backend.ts``).

Each method delegates to ``py_modules/pf_decky``. The method names, their parameters and
defaults, and the result shapes are the frontend's contract. Decky appends ``py_modules`` to
``sys.path`` and loads this file as module ``_``, so the package is importable and this file is
not: nothing may import ``main``.

The stream never starts here. The frontend runs it through Steam (``RunGame`` on a hidden
shortcut to ``bin/punktfunkrun.sh``), because gamescope only focuses and fullscreens windows
Steam launched via ``reaper`` (gamescope#484). Settings, add-host-by-address and preset editing
live in the client's own console home.
"""

import decky
from pf_decky import art, client, session, steam_input, update


class Plugin:
    # The headless CLI: build argv, run, map the exit code (pf_decky.client).

    async def discover(self) -> dict:
        return await client.discover()

    async def hosts(self) -> dict:
        return await client.hosts()

    async def pair(self, addr: str, port: int, pin: str, name: str = "Steam Deck") -> dict:
        return await client.pair(addr, port, pin, name)

    async def request_access(
        self, addr: str, port: int, name: str = "Steam Deck", fp: str = ""
    ) -> dict:
        return await client.request_access(addr, port, name, fp)

    async def device_code(self) -> dict:
        return await client.device_code()

    async def trust_host(self, addr: str, port: int, fp: str, name: str = "") -> dict:
        return await client.trust_host(addr, port, fp, name)

    async def library(self, ref: str) -> dict:
        return await client.library(ref)

    async def profiles(self, ref: str) -> dict:
        return await client.profiles(ref)

    async def wake(self, ref: str) -> dict:
        return await client.wake(ref)

    async def actions(self, ref: str) -> dict:
        return await client.actions(ref)

    async def run_action(self, ref: str, action_id: str) -> dict:
        return await client.run_action(ref, action_id)

    # Steam's side, which only a Decky plugin can reach.

    async def shortcut_art(self) -> dict:
        return await art.shortcut_art()

    async def game_art(self, appid: int, icon_hash: str = "") -> dict:
        return await art.game_art(appid, icon_hash)

    async def save_icon(self, appid: int, png_base64: str) -> dict:
        return await art.save_icon(appid, png_base64)

    async def apply_controller_config(self, name: str = "Punktfunk") -> dict:
        return await steam_input.apply_controller_config(name)

    async def runner_info(self) -> dict:
        return await client.runner_info()

    # The running stream.

    async def kill_stream(self) -> dict:
        return await session.kill_stream()

    async def stream_running(self) -> dict:
        return await session.stream_running()

    async def end_game(self, ref: str, game_id: str) -> dict:
        return await session.end_game(ref, game_id)

    async def host_action(self, action: str) -> dict:
        return await session.host_action(action)

    # Updates, for the plugin and the client.

    async def update_client(self) -> dict:
        return await update.update_client()

    async def check_update(self, force: bool = False) -> dict:
        return await update.check_update(force)

    # Decky lifecycle.

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded (runner=%s)", client._runner_path())

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading")

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
