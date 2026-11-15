//! Creating, entering and removing persistent namespaces. A namespace is
//! kept alive by bind mounting `/proc/thread-self/ns/<kind>` onto a file,
//! the same way `ip netns add` does, so no process has to hold it.

use crate::errors::{Context, Error, Result};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, setns, unshare};
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Net,
    Uts,
    Ipc,
}

impl Kind {
    fn flag(self) -> CloneFlags {
        match self {
            Kind::Net => CloneFlags::CLONE_NEWNET,
            Kind::Uts => CloneFlags::CLONE_NEWUTS,
            Kind::Ipc => CloneFlags::CLONE_NEWIPC,
        }
    }
    fn proc_name(self) -> &'static str {
        match self {
            Kind::Net => "net",
            Kind::Uts => "uts",
            Kind::Ipc => "ipc",
        }
    }
}

/// Makes `dir` a shared mount point, as iproute2 does for /run/netns, so
/// namespace mounts under it propagate to every mount namespace (the
/// docker, containerd and kiri daemons included) and unmount cleanly.
pub fn ensure_shared_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).ctx(|| format!("create {}", dir.display()))?;
    let flags = MsFlags::MS_SHARED | MsFlags::MS_REC;
    match mount(None::<&str>, dir, Some("none"), flags, None::<&str>) {
        Ok(()) => Ok(()),
        Err(nix::Error::EINVAL) => {
            mount(
                Some(dir),
                dir,
                Some("none"),
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .ctx(|| format!("bind mount {} onto itself", dir.display()))?;
            mount(None::<&str>, dir, Some("none"), flags, None::<&str>)
                .ctx(|| format!("make {} shared", dir.display()))
        }
        Err(e) => Err(e).ctx(|| format!("make {} shared", dir.display())),
    }
}

/// Creates a namespace of `kind` pinned at `path`. `init` runs inside the
/// new namespace (on a short lived thread) before it is pinned, which is
/// where the UTS hostname is set.
pub fn create(kind: Kind, path: &Path, init: impl FnOnce() -> Result<()> + Send) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                Error::network(format!("{} already exists", path.display()))
            }
            _ => Error::Io {
                context: format!("create {}", path.display()),
                source: e,
            },
        })?;
    let result = std::thread::scope(|s| {
        s.spawn(|| -> Result<()> {
            unshare(kind.flag()).ctx(|| format!("unshare {} namespace", kind.proc_name()))?;
            init()?;
            let src = format!("/proc/thread-self/ns/{}", kind.proc_name());
            mount(
                Some(src.as_str()),
                path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .ctx(|| format!("pin {} namespace at {}", kind.proc_name(), path.display()))
        })
        .join()
        .unwrap_or_else(|_| Err(Error::network("namespace thread panicked")))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Unpins and removes a namespace file. Missing files are fine.
pub fn remove(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match umount2(path, MntFlags::MNT_DETACH) {
        Ok(()) | Err(nix::Error::EINVAL) | Err(nix::Error::ENOENT) => {}
        Err(e) => return Err(e).ctx(|| format!("unmount {}", path.display())),
    }
    std::fs::remove_file(path).ctx(|| format!("remove {}", path.display()))
}

/// True when `path` is a pinned namespace (a mount of nsfs).
pub fn is_pinned(path: &Path) -> bool {
    let Ok(f) = File::open(path) else {
        return false;
    };
    nix::sys::statfs::fstatfs(f.as_fd())
        .is_ok_and(|s| s.filesystem_type() == nix::sys::statfs::NSFS_MAGIC)
}

/// Runs `f` on a fresh thread that has joined the network namespace at
/// `path`. Sockets created inside `f` stay in that namespace.
pub fn in_netns<T: Send>(path: &Path, f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    let ns = File::open(path).ctx(|| format!("open {}", path.display()))?;
    std::thread::scope(|s| {
        s.spawn(|| -> Result<T> {
            setns(ns.as_fd(), CloneFlags::CLONE_NEWNET)
                .ctx(|| format!("enter {}", path.display()))?;
            f()
        })
        .join()
        .unwrap_or_else(|_| Err(Error::network("netns thread panicked")))
    })
}

/// Moves the calling thread into the network namespace at `path`. Used by
/// helper processes (the DHCP agent) before they start any other thread.
pub fn enter_netns(path: &Path) -> Result<()> {
    let ns = File::open(path).ctx(|| format!("open {}", path.display()))?;
    setns(ns.as_fd(), CloneFlags::CLONE_NEWNET).ctx(|| format!("enter {}", path.display()))
}

/// Arranges for `cmd` to run inside the network namespace at `path`.
pub fn command_in_netns(cmd: &mut Command, path: &Path) -> Result<()> {
    let ns = File::open(path).ctx(|| format!("open {}", path.display()))?;
    // SAFETY: setns is async-signal-safe and the closure only touches the
    // already open descriptor it owns.
    unsafe {
        cmd.pre_exec(move || {
            setns(ns.as_fd(), CloneFlags::CLONE_NEWNET).map_err(std::io::Error::from)
        });
    }
    Ok(())
}
