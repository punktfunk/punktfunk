# dmgbuild settings for the host's disk image. dmg.sh passes app, background and icon with -D.
# Icon positions are icon centres in the 660 x 400 window; the background's arrow runs between.
import os.path

app = defines["app"]  # noqa: F821 (dmgbuild supplies defines)
name = os.path.basename(app)

files = [app]
symlinks = {"Applications": "/Applications"}
icon = defines.get("icon") or None  # noqa: F821
background = defines["background"]  # noqa: F821

format = "ULFO"
filesystem = "HFS+"

window_rect = ((200, 120), (660, 400))
show_toolbar = False
icon_size = 128
text_size = 13
icon_locations = {name: (165, 190), "Applications": (495, 190)}
