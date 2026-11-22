# lvim-fuzzy-plugin-devices

The **devices** plugin of the lvim fuzzy tools (`lvim-fuzzy-commander`): the disks and partitions of the machine, and the phones, shares and servers
gvfs knows, as lists to pick from and folders to go into. It lists them, mounts, unmounts, ejects, and the tool's panel goes into the folder.
`lsblk` and `udisksctl` do the disks (a mount by the desktop's rules: no root, and a password prompt when the desktop wants one); `gio` does the
gvfs side (an MTP phone, `smb://`, `sftp://`, `ftp://`, `dav://`). The plugin only answers; the tool draws.

It is a `process` plugin (JSON lines) of the `lvim-fuzzy-plugin` host and ships **inside** `lvim-fuzzy-commander`: a tool writes the plugin's
`plugin.toml` into the shared plugins directory only when its config says so — `[plugins.lvim-fuzzy-plugin-devices] install = "bundled"` (or the path
of a program that answers as the plugin).


## Installation

A plugin is a program of its own — no tool carries it. `./scripts/install.sh` builds it (`cargo build --release`), puts the program in
`~/.local/bin/` and writes its `plugin.toml` (with `exec` pointing at that program) into the shared plugins directory
`~/.config/lvim-plugins/lvim-fuzzy-plugin-devices/`; lvc, lfz and the Neovim picker find it there. The old `install = "bundled"` lines of a tool's
config are gone (a tool refuses to start with one).

## Where it is used

| App | What for |
|---|---|
| **lvc** | the `g` group: disks, partitions, phones and shares — list, mount, unmount, eject, connect to a URI |
| lfz, the Neovim picker | not used (the plugin is not loaded there) |

## Its buttons

They are added to the group `g` (go) by `plugin.toml` — the plugin's say, not the app's:

| keys | what |
|---|---|
| `gd` | **devices**: every disk with a file system, every gvfs mount, and a phone or a camera that is plugged in but not mounted yet (`⏏ … ○`); `●` mounted (and where) or `○` not; Enter mounts what is not and goes into it. The list is read each time, so a stick or a phone plugged in a moment ago is in it (a phone must be in **file transfer** mode, and gvfs needs its `gvfs-mtp` / `gvfs-gphoto2` backends) |
| `gu` | **unmount**: what a person plugged in (removable disks, what the desktop mounted under `/run/media` or `/media`) and gvfs mounts — not the system's (`/`, `/boot`, `/home` …), and not the machine's own disks unless `unmount_internal = true` |
| `ge` | **eject**: a stick, a card, an external disk (its partitions are unmounted, then it is powered off), a gvfs mount |
| `gC` | **connect**: asks for a URI (`smb://host/share`, `sftp://user@host`) and mounts it with gvfs, then goes into it |

The lists have the search field of the tool's lists. Move a button in the app's config: `[plugins.lvim-fuzzy-plugin-devices.keys] devices = "g v"`;
a key somebody has is an error and the app does not start.

## Settings

`timeout` (seconds a call may take — a mount may wait for a password prompt; the plugin tells the host to wait) and `system` (the mount points that
are not offered to unmount or eject), in `plugin.toml`, overridden in `~/.config/lvim-plugins/lvim-fuzzy-plugin-devices.toml` or under
`[plugins.lvim-fuzzy-plugin-devices]` in the tool's config.

## License

BSD-3-Clause
