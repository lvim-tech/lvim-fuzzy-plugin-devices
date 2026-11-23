//! lvim-fuzzy-plugin-devices: the disks, partitions, phones, shares and servers of the machine, as lists to pick from and folders to go into.
//! A PROCESS plugin (JSON lines on stdin / stdout) of a tool's buttons: the tool calls one of its functions and gets instructions back. `lsblk`
//! and `udisksctl` do the disks (a mount by the desktop's rules: no root, a password prompt when the desktop wants one), `gio` the gvfs side
//! (an MTP phone, `smb://`, `sftp://`, `ftp://`). The tool draws; nothing here is drawn.
//!
//! | call | context | answer (instructions) |
//! |---|---|---|
//! | `devices` | — | `menu`: every disk with a file system and every gvfs mount; Enter calls `enter` with the item; `m` mounts it, `u` unmounts it, `e` ejects it (the list stays open) |
//! | `enter` | `arg` | mounts it when it is not mounted, then `cd` into it |
//! | `list_mount` / `list_unmount` / `list_eject` | `arg` | the keys of that list: do it (`message`), then the `menu` again |
//! | `mount_only` / `unmount` / `eject` | `arg` | `message` |
//! | `connect_ask` | — | `input` for a URI (`smb://host/share`); then `connect` |
//! | `connect` | `arg` | mounts the URI with gvfs, then `cd` into it |
//!
//! An item is named `blk:/dev/sdb1` or `gio:smb://host/share`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// A block device with a file system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blk {
    pub path: String,
    pub label: String,
    pub fstype: String,
    pub size: String,
    pub mounts: Vec<String>,
    /// removable or hot-plugged (a stick, a card, an external disk)
    pub removable: bool,
    /// the disk it is a part of (`/dev/sdb`), for an eject
    pub disk: String,
}

/// A gvfs mount (a phone, a share, a server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gio {
    pub name: String,
    pub uri: String,
}

/// File systems that are no place to go into: swap, a container that is locked or belongs to a volume manager.
const NOT_FILESYSTEMS: [&str; 5] = ["swap", "crypto_LUKS", "LVM2_member", "linux_raid_member", "zfs_member"];

/// The file systems in `lsblk -J -o NAME,PATH,LABEL,FSTYPE,SIZE,MOUNTPOINTS,RM,HOTPLUG,TYPE,PKNAME,MODEL` output.
pub fn parse_lsblk(json: &str) -> Vec<Blk> {
    fn walk(v: &Value, disk: &str, removable: bool, out: &mut Vec<Blk>) {
        let Some(a) = v.as_array() else { return };
        for d in a {
            let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
            let path = s("path");
            let rm = d["rm"].as_bool().unwrap_or(false) || d["hotplug"].as_bool().unwrap_or(false) || d["rm"].as_str() == Some("1") || d["hotplug"].as_str() == Some("1");
            let kind = s("type");
            let this_disk = if kind == "disk" || kind == "rom" || kind == "loop" { path.clone() } else { disk.to_string() };
            let fstype = s("fstype");
            if !fstype.is_empty() && !NOT_FILESYSTEMS.contains(&fstype.as_str()) && !kind.starts_with("zram") && !path.contains("/zram") {
                let mounts: Vec<String> = d["mountpoints"].as_array().map(|m| m.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_else(|| d["mountpoint"].as_str().map(|m| vec![m.to_string()]).unwrap_or_default());
                out.push(Blk {
                    path: path.clone(),
                    label: if s("label").is_empty() { s("model") } else { s("label") },
                    fstype,
                    size: s("size"),
                    mounts,
                    removable: removable || rm,
                    disk: this_disk.clone(),
                });
            }
            walk(&d["children"], &this_disk, removable || rm, out);
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<Value>(json) {
        walk(&v["blockdevices"], "", false, &mut out);
    }
    // a volume of a manager (LVM) shows under every disk it spans: once
    let mut seen: Vec<String> = Vec::new();
    out.retain(|b| {
        let new = !seen.contains(&b.path);
        seen.push(b.path.clone());
        new
    });
    out
}

/// The gvfs mounts that are not plain folders (`Mount(0): Galaxy -> mtp://…`) in `gio mount -l` output. gvfs lists one mount twice when it also
/// belongs to a volume (a phone: `Mount(0): SAMSUNG Android` under its volume and `Mount(1): mtp` on its own, the same URI): the first, which has the
/// volume's name, is the one kept.
pub fn parse_gio(text: &str) -> Vec<Gio> {
    let all: Vec<Gio> = text
        .lines()
        .filter_map(|l| {
            let rest = l.trim_start().strip_prefix("Mount(")?;
            let (_, rest) = rest.split_once("): ")?;
            let (name, uri) = rest.rsplit_once(" -> ")?;
            let uri = uri.trim();
            (!uri.starts_with("file://") && uri.contains("://")).then(|| Gio { name: name.trim().to_string(), uri: uri.to_string() })
        })
        .collect();
    let mut out: Vec<Gio> = Vec::new();
    for g in all {
        if !out.iter().any(|o| o.uri.trim_end_matches('/') == g.uri.trim_end_matches('/')) {
            out.push(g);
        }
    }
    out
}

/// The gvfs VOLUMES that are not disks of udisks and are not mounted yet — a phone in file-transfer mode (MTP), a camera, an iPhone — in
/// `gio mount -l` output: a `Volume(N): name` block with an `activation_root=` URI that can be mounted. Enter mounts them.
pub fn parse_gio_volumes(text: &str) -> Vec<Gio> {
    let mut out: Vec<Gio> = Vec::new();
    // (name, is it a udisks volume — those are the disks lsblk lists —, can it be mounted, the URI)
    let mut cur: Option<(String, bool, bool, Option<String>)> = None;
    let flush = |cur: &mut Option<(String, bool, bool, Option<String>)>, out: &mut Vec<Gio>| {
        if let Some((name, udisks, can, Some(uri))) = cur.take()
            && !udisks
            && can
            && !uri.starts_with("file://")
            && uri.contains("://")
        {
            out.push(Gio { name, uri });
        }
    };
    for l in text.lines() {
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("Volume(") {
            flush(&mut cur, &mut out);
            let name = rest.split_once("): ").map_or(rest, |(_, n)| n).trim().to_string();
            cur = Some((name, false, false, None));
        } else if t.starts_with("Mount(") || t.starts_with("Drive(") {
            flush(&mut cur, &mut out);
        } else if let Some(c) = cur.as_mut() {
            if t.starts_with("Type:") && t.contains("UDisks2") {
                c.1 = true;
            } else if t.starts_with("can_mount=") {
                c.2 = t.ends_with('1');
            } else if let Some(u) = t.strip_prefix("activation_root=") {
                c.3 = Some(u.trim().to_string());
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn gio_volumes() -> Vec<Gio> {
    // `-i` adds what a volume is made of: the `activation_root=` URI (without it a phone has no address to mount)
    let o = Command::new("gio").args(["mount", "-li"]).output();
    o.ok().filter(|o| o.status.success()).map(|o| parse_gio_volumes(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

fn lsblk() -> Vec<Blk> {
    let o = Command::new("lsblk").args(["-J", "-o", "NAME,PATH,LABEL,FSTYPE,SIZE,MOUNTPOINTS,RM,HOTPLUG,TYPE,PKNAME,MODEL"]).output();
    o.ok().filter(|o| o.status.success()).map(|o| parse_lsblk(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

fn gio_mounts() -> Vec<Gio> {
    let o = Command::new("gio").args(["mount", "-l"]).output();
    o.ok().filter(|o| o.status.success()).map(|o| parse_gio(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

/// Where gvfs puts its mounts as folders: `$XDG_RUNTIME_DIR/gvfs`.
fn gvfs_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("gvfs")).unwrap_or_else(|| PathBuf::from("/run/user/1000/gvfs"))
}

fn gvfs_entries() -> Vec<String> {
    std::fs::read_dir(gvfs_dir()).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

/// The folder gvfs made for a URI: a name that appeared since `before`, else one that fits the URI (`smb://h/s` is `smb-share:server=h,share=s`).
pub fn gvfs_folder(uri: &str, before: &[String], now: &[String]) -> Option<String> {
    if let Some(n) = now.iter().find(|n| !before.contains(n)) {
        return Some(n.clone());
    }
    let (scheme, rest) = uri.split_once("://")?;
    let mut parts = rest.trim_end_matches('/').splitn(2, '/');
    let host = parts.next().unwrap_or("");
    let share = parts.next().unwrap_or("");
    let want = match scheme {
        "smb" => format!("smb-share:server={host},share={share}"),
        "mtp" => format!("mtp:host={host}"),
        s => format!("{s}:host={host}"),
    };
    now.iter().find(|n| n.eq_ignore_ascii_case(&want) || n.starts_with(&want)).cloned()
}

/// `Mounted /dev/sdb1 at /run/media/me/LABEL.` → the folder.
pub fn mounted_at(out: &str) -> Option<String> {
    let l = out.lines().find(|l| l.contains(" at "))?;
    let (_, at) = l.split_once(" at ")?;
    Some(at.trim().trim_end_matches('.').to_string())
}

fn first_line(e: &str) -> String {
    e.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").to_string()
}

/// Run a command in a terminal of its own (a pseudo terminal): a question for a password — polkit's text agent, `gio mount` of a share — is not
/// written on the tool's screen but asked with the prompt of this machine (the `ward` / `pinentry` of `~/.gnupg/gpg-agent.conf`, `$SSH_ASKPASS`;
/// see `lvim_fuzzy_plugin::askpass`), and the answer goes back to the command. A desktop's own authentication popup (a graphical polkit agent) is
/// used by the command as usual when there is one. `id` is the call's, for a keepalive (a person is waited for).
pub fn exec(id: &Value, cfg: &Value, cmd: &str, args: &[&str]) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    let (mut master, slave) = {
        let (mut m, mut sl) = (0i32, 0i32);
        // SAFETY: openpty fills the two descriptors; nothing else is touched
        if unsafe { libc::openpty(&mut m, &mut sl, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } != 0 {
            return Err("no terminal to run it in".into());
        }
        // SAFETY: the descriptors are ours and open
        unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(sl)) }
    };
    let dup = |f: &std::fs::File| -> std::io::Result<std::process::Stdio> { Ok(std::process::Stdio::from(f.try_clone()?)) };
    let mut c = Command::new(cmd);
    c.args(args).stdin(dup(&slave).map_err(|e| e.to_string())?).stdout(dup(&slave).map_err(|e| e.to_string())?).stderr(dup(&slave).map_err(|e| e.to_string())?);
    // SAFETY: setsid and ioctl on stdin (the slave) are async-signal-safe: the slave becomes the command's controlling terminal
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = c.spawn().map_err(|e| format!("{cmd}: {e}"))?;
    drop(c);
    drop(slave);
    // what the command writes arrives on a channel
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut reader = master.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let limit = std::time::Instant::now() + Duration::from_secs(cfg["timeout"].as_u64().unwrap_or(120).clamp(5, 3600));
    let mut text = String::new();
    let mut answered = 0usize;
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        // a question that waits for a line: the text ends with `:` (and a space) after the last line break
        let tail = text.rsplit('\n').next().unwrap_or("").trim_end().to_string();
        // (not asked twice about the same question: something new must have been written since the answer)
        if text.len() > answered && tail.ends_with(':') && tail.to_lowercase().contains("password") {
            keepalive(id);
            let what = text.lines().rev().find(|l| l.contains("required") || l.contains("Authenticat")).unwrap_or("A password is needed");
            let said = clean(what);
            match lvim_fuzzy_plugin::askpass::ask(cfg, "lvim — authentication", &format!("{said}\n{}", clean(&tail))) {
                Ok(secret) => {
                    let _ = master.write_all(format!("{secret}\n").as_bytes());
                    answered = text.len();
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("cancelled".into());
                }
            }
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < limit => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{cmd}: no answer in time"));
            }
        }
    };
    // what it wrote last
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    drop(master);
    let text = clean(&text);
    // the password question and the line the terminal echoed are not the answer
    let shown: String = text.lines().filter(|l| !l.trim().is_empty() && !l.contains("====") && !l.to_lowercase().trim_end().ends_with("password:")).collect::<Vec<_>>().join("\n");
    if status.success() {
        Ok(shown)
    } else {
        let said = first_line(&shown);
        let low = said.to_lowercase();
        Err(if low.contains("not authorized") || low.contains("authentication") || low.contains("dismissed") {
            format!("{said} — the system did not allow it (a wrong password, or a disk of the system that needs an administrator)")
        } else {
            said
        })
    }
}

/// The text a terminal gave back without its carriage returns and escape sequences.
fn clean(t: &str) -> String {
    let mut out = String::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {}
            '\u{1b}' => {
                // an escape sequence: up to its final letter
                if it.peek() == Some(&'[') {
                    it.next();
                    for d in it.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn is_system(b: &Blk, system: &[String]) -> bool {
    b.mounts.iter().any(|m| system.iter().any(|s| m == s || (s != "/" && m.starts_with(&format!("{s}/")))))
}

fn config_system(cfg: &Value) -> Vec<String> {
    cfg["system"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_else(|| ["/", "/boot", "/efi", "/home", "/usr", "/var", "/nix", "[SWAP]"].map(String::from).to_vec())
}

/// An item whose columns the tool aligns one under another, each in its colour (`label` is the text a search matches).
fn row(cells: Vec<(String, &str)>, call: &str, arg: &str) -> Value {
    let label: Vec<&str> = cells.iter().map(|(t, _)| t.as_str()).collect();
    json!({"label": label.join("  "), "call": call, "arg": arg, "cells": cells.iter().map(|(t, c)| json!([t, c])).collect::<Vec<_>>()})
}

/// The columns of a disk: a mark for a removable one, the device, its label, the file system, the size, and where it is mounted.
fn blk_cells(b: &Blk) -> Vec<(String, &'static str)> {
    let state = match b.mounts.iter().find(|m| *m != "[SWAP]") {
        Some(m) => (format!("● {m}"), "green"),
        None => ("○".to_string(), "comment"),
    };
    vec![
        (if b.removable { "⏏" } else { " " }.to_string(), "orange"),
        (b.path.clone(), "blue"),
        (b.label.clone(), "yellow"),
        (b.fstype.clone(), "teal"),
        (b.size.clone(), "cyan"),
        state,
    ]
}

fn gio_cells(kind: &str, g: &Gio, mounted: bool) -> Vec<(String, &'static str)> {
    vec![
        (if mounted { " " } else { "⏏" }.to_string(), "orange"),
        (kind.to_string(), "purple"),
        (g.name.clone(), "yellow"),
        (g.uri.clone(), "blue"),
        ((if mounted { "● mounted" } else { "○" }).to_string(), if mounted { "green" } else { "comment" }),
    ]
}

/// The list of the devices: each row has `m` (mount) when it is not mounted, `u` (unmount) and `e` (eject) when it is — they do it and the list
/// comes back refreshed, so the list stays open.
fn devices_menu(system: &[String]) -> Vec<Value> {
    let mut items: Vec<Value> = lsblk()
        .iter()
        .map(|b| {
            let arg = format!("blk:{}", b.path);
            let is_mounted = b.mounts.iter().any(|m| m != "[SWAP]");
            // the machine's own system disks are not offered to unmount (the same rule as the unmount list)
            let can = is_mounted && !is_system(b, system);
            with_keys(row(blk_cells(b), "enter", &arg), &arg, !is_mounted, can, can && b.removable)
        })
        .collect();
    let mounted = gio_mounts();
    for g in &mounted {
        let arg = format!("gio:{}", g.uri);
        items.push(with_keys(row(gio_cells("gvfs", g, true), "enter", &arg), &arg, false, true, true));
    }
    // a phone or a camera that is plugged in and not mounted yet
    for g in gio_volumes().iter().filter(|v| !mounted.iter().any(|m| m.uri.trim_end_matches('/') == v.uri.trim_end_matches('/'))) {
        let arg = format!("gio:{}", g.uri);
        items.push(with_keys(row(gio_cells("phone/camera", g, false), "enter", &arg), &arg, true, false, false));
    }
    if items.is_empty() {
        return vec![message("no disks or mounts found (lsblk, gio)")];
    }
    vec![menu("Devices", items)]
}

/// The keys of a row of the devices list besides Enter.
fn with_keys(mut row: Value, arg: &str, mount: bool, unmount: bool, eject: bool) -> Value {
    let mut keys = Vec::new();
    if mount {
        keys.push(json!({"key": "m", "label": "mount", "call": "list_mount", "arg": arg}));
    }
    if unmount {
        keys.push(json!({"key": "u", "label": "unmount", "call": "list_unmount", "arg": arg}));
    }
    if eject {
        keys.push(json!({"key": "e", "label": "eject", "call": "list_eject", "arg": arg}));
    }
    row["keys"] = json!(keys);
    row
}

fn message(t: &str) -> Value {
    json!({"op": "message", "text": t})
}

fn menu(title: &str, items: Vec<Value>) -> Value {
    json!({"op": "menu", "title": title, "items": items})
}

/// Tell the host a call is waiting for a person (a password prompt of the desktop).
fn keepalive(id: &Value) {
    println!("{}", json!({"id": id, "keepalive": 120}));
}

/// The folder to go into, from the mount's answer.
fn cd(path: &str) -> Value {
    json!({"op": "cd", "path": path})
}

fn blk_of(arg: &str) -> Option<Blk> {
    let p = arg.strip_prefix("blk:")?;
    lsblk().into_iter().find(|b| b.path == p)
}

/// The answer to one call (`id`: the call's, for a keepalive).
pub fn answer(id: &Value, ctx: &Value) -> Result<Vec<Value>, String> {
    let call = ctx["call"].as_str().unwrap_or("");
    let arg = ctx["arg"].as_str().unwrap_or("");
    let system = config_system(&ctx["config"]);
    match call {
        "devices" => Ok(devices_menu(&system)),
        "enter" => {
            if let Some(uri) = arg.strip_prefix("gio:") {
                return connect(id, &ctx["config"], uri);
            }
            let b = blk_of(arg).ok_or_else(|| format!("{arg}: not found any more"))?;
            if let Some(m) = b.mounts.iter().find(|m| *m != "[SWAP]") {
                return Ok(vec![cd(m)]);
            }
            keepalive(id);
            let out = exec(id, &ctx["config"], "udisksctl", &["mount", "-b", &b.path])?;
            let at = mounted_at(&out).ok_or_else(|| format!("mounted, but not where: {}", first_line(&out)))?;
            Ok(vec![cd(&at)])
        }
        "unmount" => {
            keepalive(id);
            if let Some(uri) = arg.strip_prefix("gio:") {
                exec(id, &ctx["config"], "gio", &["mount", "-u", uri])?;
                return Ok(vec![message(&format!("unmounted {uri}"))]);
            }
            let b = blk_of(arg).ok_or_else(|| format!("{arg}: not found any more"))?;
            exec(id, &ctx["config"], "udisksctl", &["unmount", "-b", &b.path])?;
            Ok(vec![message(&format!("unmounted {}", b.path))])
        }
        "eject" => {
            keepalive(id);
            if let Some(uri) = arg.strip_prefix("gio:") {
                exec(id, &ctx["config"], "gio", &["mount", "-e", uri])?;
                return Ok(vec![message(&format!("ejected {uri}"))]);
            }
            let b = blk_of(arg).ok_or_else(|| format!("{arg}: not found any more"))?;
            // every partition of the disk is unmounted first, then the disk is powered off
            for p in lsblk().iter().filter(|p| p.disk == b.disk && p.mounts.iter().any(|m| m != "[SWAP]")) {
                exec(id, &ctx["config"], "udisksctl", &["unmount", "-b", &p.path])?;
            }
            exec(id, &ctx["config"], "udisksctl", &["power-off", "-b", &b.disk])?;
            Ok(vec![message(&format!("{} can be taken out", b.disk))])
        }
        // the keys of the devices list: do it, then the list again (refreshed), so it stays open
        "list_mount" | "list_unmount" | "list_eject" => {
            keepalive(id);
            let what = match call {
                "list_mount" => "mount_only",
                "list_unmount" => "unmount",
                _ => "eject",
            };
            let mut ctx2 = ctx.clone();
            ctx2["call"] = json!(what);
            // a failure is told, and the list comes back all the same
            let mut out = answer(id, &ctx2).unwrap_or_else(|e| vec![json!({"op": "error", "text": e})]);
            out.extend(devices_menu(&system));
            Ok(out)
        }
        "mount_only" => {
            if let Some(uri) = arg.strip_prefix("gio:") {
                exec(id, &ctx["config"], "gio", &["mount", uri])?;
                return Ok(vec![message(&format!("mounted {uri}"))]);
            }
            let b = blk_of(arg).ok_or_else(|| format!("{arg}: not found any more"))?;
            let out = exec(id, &ctx["config"], "udisksctl", &["mount", "-b", &b.path])?;
            Ok(vec![message(&format!("mounted {} at {}", b.path, mounted_at(&out).unwrap_or_default()))])
        }
        "connect_ask" => Ok(vec![json!({"op": "input", "prompt": "Connect to (smb://host/share, sftp://user@host, ftp://host, dav://…)", "value": "", "then": "connect"})]),
        "connect" => connect(id, &ctx["config"], arg.trim()),
        _ => Err(format!("no such function: {call}")),
    }
}

/// Mount a URI with gvfs and go into the folder it made.
fn connect(id: &Value, cfg: &Value, uri: &str) -> Result<Vec<Value>, String> {
    if !uri.contains("://") {
        return Err("a URI like smb://host/share".into());
    }
    let before = gvfs_entries();
    // already mounted: its folder is there
    if let Some(f) = gvfs_folder(uri, &before, &before) {
        return Ok(vec![cd(&gvfs_dir().join(f).to_string_lossy())]);
    }
    keepalive(id);
    exec(id, cfg, "gio", &["mount", uri])?;
    let now = gvfs_entries();
    let f = gvfs_folder(uri, &before, &now).ok_or_else(|| format!("{uri}: mounted, but its folder is not in {}", gvfs_dir().display()))?;
    Ok(vec![cd(&gvfs_dir().join(f).to_string_lossy())])
}

/// The plugin's loop: JSON lines on stdin, answers on stdout; each call on a thread, with its own time limit (`config.timeout` seconds).
pub fn run() {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let inflight = Arc::new(AtomicUsize::new(0));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        match req["method"].as_str() {
            Some("call") => {
                let id = req["id"].clone();
                let params = req["params"].clone();
                let limit = Duration::from_secs(params["config"]["timeout"].as_u64().unwrap_or(120).clamp(1, 3600));
                let (tx, rx) = mpsc::channel();
                let id2 = id.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(answer(&id2, &params));
                });
                let out = out.clone();
                inflight.fetch_add(1, Ordering::SeqCst);
                let inflight = inflight.clone();
                std::thread::spawn(move || {
                    let reply = match rx.recv_timeout(limit) {
                        Ok(Ok(list)) => json!({"id": id, "result": list}),
                        Ok(Err(e)) => json!({"id": id, "error": e}),
                        Err(_) => json!({"id": id, "error": format!("no answer in {} s", limit.as_secs())}),
                    };
                    if let Ok(mut o) = out.lock() {
                        let _ = writeln!(o, "{reply}");
                        let _ = o.flush();
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Some("event") if req["params"]["event"] == "quit" => break,
            _ => {}
        }
    }
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while inflight.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"sda","path":"/dev/sda","label":null,"fstype":null,"size":"931.5G","mountpoints":[],"rm":false,"hotplug":false,"type":"disk","pkname":null,"model":"BIGDISK","children":[
        {"name":"sda1","path":"/dev/sda1","label":null,"fstype":"swap","size":"8G","mountpoints":["[SWAP]"],"rm":false,"hotplug":false,"type":"part","pkname":"sda","model":null},
        {"name":"sda2","path":"/dev/sda2","label":"DATA","fstype":"ext4","size":"900G","mountpoints":["/home"],"rm":false,"hotplug":false,"type":"part","pkname":"sda","model":null}]},
      {"name":"sdb","path":"/dev/sdb","label":null,"fstype":null,"size":"16G","mountpoints":[],"rm":true,"hotplug":true,"type":"disk","pkname":null,"model":"STICK","children":[
        {"name":"sdb1","path":"/dev/sdb1","label":"RECOV","fstype":"vfat","size":"16G","mountpoints":[],"rm":true,"hotplug":true,"type":"part","pkname":"sdb","model":null},
        {"name":"sdb2","path":"/dev/sdb2","label":null,"fstype":"crypto_LUKS","size":"1G","mountpoints":[],"rm":true,"hotplug":true,"type":"part","pkname":"sdb","model":null}]},
      {"name":"zram0","path":"/dev/zram0","label":null,"fstype":"swap","size":"4G","mountpoints":["[SWAP]"],"rm":false,"hotplug":false,"type":"disk","pkname":null,"model":null}]}"#;

    #[test]
    fn disks_with_a_file_system_are_listed_and_the_rest_is_not() {
        let v = parse_lsblk(LSBLK);
        // swap and a locked container are no places to go into
        assert_eq!(v.iter().map(|b| b.path.as_str()).collect::<Vec<_>>(), ["/dev/sda2", "/dev/sdb1"]);
        let data = &v[0];
        assert_eq!((data.label.as_str(), data.fstype.as_str(), data.removable, data.disk.as_str()), ("DATA", "ext4", false, "/dev/sda"));
        let stick = &v[1];
        assert!(stick.removable && stick.mounts.is_empty() && stick.disk == "/dev/sdb");
        assert!(is_system(data, &config_system(&Value::Null)), "/home is the system's");
        assert!(!is_system(stick, &config_system(&Value::Null)));
        assert_eq!(blk_cells(stick).last().unwrap().0, "○");
        assert_eq!(blk_cells(data).last().unwrap().0, "● /home");
        // a volume that shows under two disks is one
        let twice = r#"{"blockdevices":[{"name":"a","path":"/dev/a","type":"disk","children":[{"name":"v","path":"/dev/mapper/v","fstype":"xfs","type":"lvm","mountpoints":[]}]},{"name":"b","path":"/dev/b","type":"disk","children":[{"name":"v","path":"/dev/mapper/v","fstype":"xfs","type":"lvm","mountpoints":[]}]}]}"#;
        assert_eq!(parse_lsblk(twice).len(), 1);
    }

    #[test]
    fn gvfs_mounts_and_folders_are_found() {
        let g = parse_gio("Drive(0): Disk\n  Type: GProxyDrive\nMount(0): Galaxy -> mtp://SAMSUNG_X/\n  Type: GDaemonMount\nMount(1): docs on server -> smb://server/docs\nMount(2): local -> file:///mnt/x\n");
        assert_eq!(g, vec![Gio { name: "Galaxy".into(), uri: "mtp://SAMSUNG_X/".into() }, Gio { name: "docs on server".into(), uri: "smb://server/docs".into() }]);
        let before = vec!["mtp:host=A".to_string()];
        let now = vec!["mtp:host=A".to_string(), "smb-share:server=server,share=docs".to_string()];
        // what appeared
        assert_eq!(gvfs_folder("smb://server/docs", &before, &now).as_deref(), Some("smb-share:server=server,share=docs"));
        // already there: by the URI
        assert_eq!(gvfs_folder("mtp://A/", &now, &now).as_deref(), Some("mtp:host=A"));
        assert_eq!(gvfs_folder("sftp://nowhere", &now, &now), None);
    }

    #[test]
    fn a_phone_that_is_plugged_in_and_not_mounted_is_found() {
        let text = "Drive(0): Disk\n  Type: GProxyDrive (GProxyVolumeMonitorUDisks2)\n  Volume(0): 700 GB\n    Type: GProxyVolume (GProxyVolumeMonitorUDisks2)\n    can_mount=1\nVolume(0): Galaxy S21\n  Type: GProxyVolume (GProxyVolumeMonitorMTP)\n  ids:\n   unix-device: '/dev/bus/usb/001/004'\n  can_mount=1\n  can_eject=0\n  should_automount=1\n  activation_root=mtp://SAMSUNG_Android_RF8N/\nVolume(1): RECOV\n  Type: GProxyVolume (GProxyVolumeMonitorUDisks2)\n  can_mount=1\n  activation_root=file:///run/media/me/RECOV\nMount(0): Camera -> gphoto2://Canon_EOS/\n  Type: GDaemonMount\n";
        // the udisks volumes are the disks lsblk lists; a mount is already mounted; what is left is the phone
        assert_eq!(parse_gio_volumes(text), vec![Gio { name: "Galaxy S21".into(), uri: "mtp://SAMSUNG_Android_RF8N/".into() }]);
        assert_eq!(parse_gio_volumes("Volume(0): Nothing\n  can_mount=0\n  activation_root=mtp://x/\n"), vec![]);
    }

    #[test]
    fn a_question_for_a_password_goes_to_the_prompt_of_the_machine_and_the_answer_back() {
        let id = json!(1);
        // the command asks on its terminal; the settings' askpass is the prompt of this test machine
        let cfg = json!({"askpass": "printf s3cret", "timeout": 20});
        let out = exec(&id, &cfg, "sh", &["-c", "printf 'Authentication is required to mount X\\nPassword: '; read -r p </dev/tty; echo got-$p"]).unwrap();
        assert!(out.contains("got-s3cret"), "{out:?}");
        // a refused one is an error with what the command said
        let cfg = json!({"askpass": "false", "timeout": 20});
        assert_eq!(exec(&id, &cfg, "sh", &["-c", "printf 'Password: '; read -r p </dev/tty; echo got-$p"]).unwrap_err(), "cancelled");
        // a command that fails says why
        let e = exec(&id, &json!({}), "sh", &["-c", "echo 'Error: not authorized'; exit 1"]).unwrap_err();
        assert!(e.contains("not authorized") && e.contains("administrator"), "{e}");
        // a command that asks nothing just runs
        assert_eq!(exec(&id, &json!({}), "sh", &["-c", "echo fine"]).unwrap().trim(), "fine");
    }

    #[test]
    fn what_udisks_says_is_read() {
        assert_eq!(mounted_at("Mounted /dev/sdb1 at /run/media/me/RECOV.\n").as_deref(), Some("/run/media/me/RECOV"));
        assert_eq!(mounted_at("Mounted /dev/sdb1 at /run/media/me/MY DISK.").as_deref(), Some("/run/media/me/MY DISK"));
        assert_eq!(mounted_at("nothing"), None);
        assert_eq!(first_line("\n  Error mounting: not authorized \nmore"), "Error mounting: not authorized");
    }

    #[test]
    fn phone_volume_from_gio_li() {
        let text = "Volume(0): SAMSUNG Android\n  Type: GProxyVolume (GProxyVolumeMonitorMTP)\n  ids:\n   unix-device: '/dev/bus/usb/001/003'\n  activation_root=mtp://SAMSUNG_SAMSUNG_Android_X/\n  can_mount=1\n  can_eject=0\nVolume(1): RECOV\n  Type: GProxyVolume (GProxyVolumeMonitorUDisks2)\n  can_mount=1\n";
        let v = parse_gio_volumes(text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "SAMSUNG Android");
        assert_eq!(v[0].uri, "mtp://SAMSUNG_SAMSUNG_Android_X/");
    }

    #[test]
    fn one_mount_listed_twice_is_one() {
        let text = "Volume(0): SAMSUNG Android\n  Mount(0): SAMSUNG Android -> mtp://X_Y/\nMount(1): mtp -> mtp://X_Y/\nMount(2): EFI -> file:///run/media/u/EFI\n";
        let m = parse_gio(text);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "SAMSUNG Android");
    }

    #[test]
    fn the_buttons_answer_with_instructions() {
        let id = json!(1);
        let r = answer(&id, &json!({"call": "connect_ask"})).unwrap();
        assert_eq!(r[0]["op"], "input");
        assert_eq!(r[0]["then"], "connect");
        assert!(answer(&id, &json!({"call": "connect", "arg": "not a uri"})).is_err());
        assert!(answer(&id, &json!({"call": "nope"})).is_err());
        // the list is a menu (or a message on a machine with no disks): never a crash
        let r = answer(&id, &json!({"call": "devices"})).unwrap();
        assert!(r[0]["op"] == "menu" || r[0]["op"] == "message");
        // the items carry their columns, each with a colour
        if r[0]["op"] == "menu" {
            assert!(r[0]["items"][0]["cells"].as_array().is_some_and(|c| c.len() >= 5 && c[1][1].is_string()));
        }
    }
}
