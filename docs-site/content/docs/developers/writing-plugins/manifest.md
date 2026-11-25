---
title: Manifest and sandbox
description: What a plugin declares in its package.json, what its sandbox can never reach, how the host builds a game's command, and how a plugin asks for folders it could not know.
---

Declare what your plugin needs, and learn what it can never reach whatever it declares. The
manifest is a `punktfunk` block in the plugin's `package.json`; the
[minimal plugin](/docs/developers/writing-plugins#a-minimal-library-plugin) has one. It ships
inside the reviewed tarball, so nothing the plugin does at runtime widens it. Without it, a
plugin does not start.

## Fields

| Field | Meaning |
|---|---|
| `schema` | `1`. Any other value is refused. |
| `id` | The plugin id: `[a-z][a-z0-9-]*`, at most 64 characters. Use the same string as the plugin's `name`; it keys the plugin's token, state dir and library provider. |
| `reads` | Paths the plugin reads. Absolute or `~/`-rooted. A missing path is skipped. |
| `files` | Folders the plugin reads the files of, nothing below them. Use it for a folder that holds a game tree: on Windows a `reads` entry rewrites every file below it, and a `files` entry lands at once. One `*` component names each folder under it: `%ProgramFiles%/EA Games/*/__Installer` is every game's `__Installer`, new ones within a minute. Needs host 0.46; an older host drops the entry, so also ask for it from `wants`. |
| `writes` | Paths it also writes. On Linux each one waits for the owner's yes, except one under `/tmp`: the host asks when the plugin is installed, as a pending folder request, and the folder binds read-write once allowed. On Windows only the operator grants a write. |
| `network` | `true` to reach the network. |
| `registry` | Windows registry keys it reads, as `HKLM\Software\…` or `HKCU\Software\…`: a launcher's own key, never one that holds credentials or policy, nor a parent of one. A key under `HKCU`, like a path under `~/`, marks a source that belongs to one account; the host grants it in every signed-in account's hive. |
| `exec` | Named argv templates the host may run for this plugin's titles. |

## What the sandbox never reaches

No manifest entry and no grant opens these:

| Never | Paths |
|---|---|
| System roots | `/`, the home directory or its parents, `/proc`, `/sys`, `/dev`, `/run` (except `/run/media`) |
| Keys and the host's own files | `~/.ssh`, `~/.gnupg`, `~/.config/punktfunk` |
| Linux: a browser profile, keyring or cloud credential, or a parent of one | `~/.mozilla`, `~/.config/google-chrome`, `~/.var/app/org.mozilla.firefox`, `~/.local/share/keyrings`, `~/.aws`, `~/.kube` and the like, and so `~/.config`, `~/.local` and `~/.var/app` whole |
| Windows: the same | A profile's `AppData\Local\Google\Chrome\User Data`, `AppData\Roaming\Microsoft\Protect`, `.aws` and their kind |
| Linux: `/tmp` itself and what a desktop keeps there | `/tmp/.X11-unix`, `/tmp/tmux-*`, an SSH agent's `/tmp/ssh-*` and the like, and `~/.Xauthority`. A `writes` entry under `/tmp` binds only its own path. |

On Linux a `writes` entry or a write grant also never binds what runs at login, session start or
a compositor reload, nor a parent of it: `~/.config/systemd`, `~/.config/autostart`,
`~/.config/hypr`, `~/.local/bin`, `~/.local/share/applications`, `~/.local/share/gnome-shell`, and
the shell and X start-up files such as `~/.bashrc`, `~/.profile` and `~/.xinitrc`. Reading those
with `reads` is fine. Any other `writes` entry outside `/tmp` binds once the owner allows the
request the host files for it, and stays unbound after a no.

## Commands the host runs for you

A plugin never starts a game. It publishes titles, and the host builds each command.

An `exec` template names a program and its arguments with `{param}` placeholders. A title supplies
the values, and the host builds the argv itself. `exe` is a program name looked up on `PATH`, or
an absolute path inside the plugin's roots; it may be a `{param}` of kind `path`. `cwd` is
optional and must also lie inside the roots. Each parameter declares its kind:

| Kind | Accepts |
|---|---|
| `name` | Printable text, up to 128 characters |
| `id` | `[A-Za-z0-9._-]`, up to 128 characters |
| `digits` | ASCII digits, up to 32 |
| `path` | An absolute path inside the plugin's declared or granted roots |
| `args` | Extra argv elements. The only kind that may start with `-` or repeat |

No other value may start with `-`. The host drops a title whose values don't fit when the plugin
publishes it.

### Emulator titles

A ROM needs no template. `launch: { kind: "emulator", value: "pcsx2", args: [...] }` names a
catalog emulator, and the arguments `platform`, `file`, and optionally `core`; `file` must lie
inside the plugin's roots. The host starts `file` in its best copy of that emulator, after
first-run setup and the platform's firmware. It takes no flags: a title that needs its own goes
through a template. `GET /api/v1/emulators/catalog` lists the platforms and emulators. A plugin
that moves saves asks once with `saves: true` on `POST /api/v1/plugin-access/requests`.

## Network

A Linux plugin with `network` reaches the internet and the LAN, but not the `localhost` of the
computer the host runs on, and that computer's own LAN address leads nowhere from inside the
plugin.

To use a server there, ask for its TCP port with `ports: [8080]` on
`POST /api/v1/plugin-access/requests`. The answer for each is `granted`, `pending`, `denied` or
`refused:<rule>`, under the path `127.0.0.1:8080`, which is where the plugin connects once the
operator allows it. The host restarts the plugin on a yes. It refuses a port of its own (47980 to
48010), a plugin without `network`, and every port on Windows (`refused:unsupported`), where a
plugin's network is as it was.

`@punktfunk/plugin-kit` wraps this as `reachServer(url)`: it answers the address to dial for a
server the person configured, wherever that server runs.

## Folders you can't know in advance

A package can't know where someone keeps their ROMs or a second Steam library.

| Who knows the folder | Use |
|---|---|
| The launcher's own data, read at run time | `wants: (cfg) => [...]` on `defineLibraryPlugin` |
| A plugin without `defineLibraryPlugin` | `requestAccess(paths, reason)` from `@punktfunk/plugin-kit` |
| The operator: a save folder, a config directory | A `handedPath()` field in `game` or `config` |

**`wants`.** Before each scan the kit asks the host for every folder the plugin can't reach, then
scans what it can reach now. Return `{ path, files: true }` for a folder that holds a game tree
when its own files are all you read. For a library folder whose game folders each hold the one
file you read, return the pattern `{ path: "D:\\Games\\*", files: true }` beside the game folders
themselves: one yes covers them all, and a host before 0.46 refuses the pattern and prompts per
folder.

**A request only creates a pending row.** The operator allows it on the library source in the
console, or with `punktfunk-host plugins grant`. A grant restarts the plugin. Don't tell users to
widen the runner unit or change ACLs by hand.

**`handedPath()`.** The console grants each folder the operator adds there when the form saves,
read-only unless `handedPath({ write: true })`. A folder the plugin fills in itself is never
granted that way. On Windows a plugin's own write request is always refused; write access comes
only from the operator, and never inside Program Files or ProgramData.
