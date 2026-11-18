//! `gantry data move`: moves a service's data to the path the project file
//! now names, with rsync.
//!
//! The project file is the desired state. When a running container mounts
//! a different host path (or a named volume) at a target than the file now
//! asks for, the data has to move before the container is recreated. The
//! bulk is copied while the services run, then they are stopped for one
//! last pass, the copy is verified, and they are recreated on the new path.
//! The old data is never deleted.

use crate::engine::ContainerInfo;
use crate::errors::{Context, Error, Result};
use crate::orchestrator::{Orchestrator, UpOptions, lock};
use crate::project::MountKind;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

/// Left in a destination directory while a move into it is unfinished, so
/// `up` refuses it and a rerun of `data move` may continue it.
pub const MARKER: &str = ".gantry-move";

/// One piece of data that has to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    /// Where the data is now (a bind source or a volume's mountpoint).
    pub from: PathBuf,
    /// Where the project file now wants it.
    pub to: PathBuf,
    /// The volume name when the data is in a named volume.
    pub volume: Option<String>,
    /// Services that mount it, and where inside the container.
    pub users: Vec<(String, String)>,
}

impl Move {
    fn services(&self) -> Vec<String> {
        let mut v: Vec<String> = self.users.iter().map(|(s, _)| s.clone()).collect();
        v.dedup();
        v
    }

    fn describe(&self) -> String {
        let from = match &self.volume {
            Some(v) => format!("volume {v} ({})", self.from.display()),
            None => self.from.display().to_string(),
        };
        let users: Vec<String> = self.users.iter().map(|(s, t)| format!("{s}:{t}")).collect();
        format!("{from} -> {} [{}]", self.to.display(), users.join(", "))
    }
}

pub struct MoveOptions {
    pub services: Vec<String>,
    /// Live passes before the services are stopped.
    pub passes: u32,
    pub dry_run: bool,
    /// Copy ACLs and extended attributes (`-AX`). Off for filesystems that
    /// do not support them.
    pub xattrs: bool,
    /// Allow a destination that already holds data.
    pub force: bool,
    /// Stop timeout in seconds.
    pub timeout: Option<u64>,
}

/// Compares each service's existing container with the project file and
/// returns the data that would have to move. Services without a container
/// have nothing to move.
pub async fn plan(orch: &Orchestrator, services: &[String]) -> Result<Vec<Move>> {
    let containers: Vec<ContainerInfo> = orch
        .containers()
        .await?
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| !c.oneoff)
        .collect();
    let mut moves: IndexMap<(PathBuf, PathBuf), Move> = IndexMap::new();
    for (name, svc) in &orch.project.services {
        if !services.is_empty() && !services.contains(name) {
            continue;
        }
        let Some(c) = containers.iter().find(|c| &c.service == name) else {
            continue;
        };
        for m in &svc.mounts {
            if m.kind == MountKind::Tmpfs {
                continue;
            }
            let Some(cur) = c.mounts.iter().find(|x| x.destination == m.target) else {
                continue;
            };
            if cur.source.is_empty() || !matches!(cur.kind.as_str(), "bind" | "volume") {
                continue;
            }
            let unchanged = match m.kind {
                MountKind::Bind => {
                    cur.kind == "bind"
                        && clean(Path::new(&cur.source)) == clean(Path::new(&m.source))
                }
                MountKind::Volume => cur.kind == "volume" && cur.name.as_deref() == Some(&m.source),
                MountKind::Tmpfs => true,
            };
            if unchanged {
                continue;
            }
            if m.kind != MountKind::Bind {
                return Err(Error::config(format!(
                    "service {name}: {} now uses a named volume; data move only moves data to a host path (a bind mount)",
                    m.target
                )));
            }
            let from = clean(Path::new(&cur.source));
            let to = clean(Path::new(&m.source));
            let entry = moves.entry((from.clone(), to.clone())).or_insert(Move {
                from,
                to,
                volume: (cur.kind == "volume").then(|| cur.name.clone()).flatten(),
                users: vec![],
            });
            entry.users.push((name.clone(), m.target.clone()));
        }
    }
    let moves: Vec<Move> = moves.into_values().collect();
    for a in &moves {
        if a.to.starts_with(&a.from) || a.from.starts_with(&a.to) {
            return Err(Error::config(format!(
                "cannot move {} into {}: one is inside the other",
                a.from.display(),
                a.to.display()
            )));
        }
        if let Some(b) = moves.iter().find(|b| b.to == a.to && b.from != a.from) {
            return Err(Error::config(format!(
                "{} and {} would both move to {}",
                a.from.display(),
                b.from.display(),
                a.to.display()
            )));
        }
    }
    Ok(moves)
}

/// Called by `up`: refuses to recreate a service onto a new data path that
/// is empty or holds an unfinished move while the old path has data.
pub async fn refuse_unmoved(orch: &Orchestrator, services: &[String]) -> Result<()> {
    for m in plan(orch, services).await? {
        if has_marker(&m.to) || (is_empty(&m.to) && !is_empty(&m.from)) {
            return Err(Error::State(format!(
                "{} has moved in the project file, but the data has not: {}. Run `gantry data move` to copy it first",
                m.users
                    .iter()
                    .map(|(s, t)| format!("{s}:{t}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                m.describe()
            )));
        }
    }
    Ok(())
}

pub async fn run(orch: &Orchestrator, opts: &MoveOptions) -> Result<()> {
    orch.selected(&opts.services)?;
    let _lock = lock(&orch.project.name)?;
    let moves = plan(orch, &opts.services).await?;
    if moves.is_empty() {
        println!("Nothing to move: every mount already uses the path in the project file.");
        return Ok(());
    }
    for m in &moves {
        println!("Move {}", m.describe());
    }
    if opts.dry_run {
        return Ok(());
    }
    which_rsync()?;
    for m in &moves {
        prepare(m, opts.force)?;
    }

    // Live passes: the bulk moves while the services keep running.
    for m in &moves {
        for pass in 1..=opts.passes.max(1) {
            let st = rsync(m, opts.xattrs).await?;
            println!(
                "  pass {pass}: {} files, {} copied",
                st.files,
                human(st.bytes)
            );
            if st.files == 0 {
                break;
            }
        }
    }

    // Cutover: only the services that use the data stop.
    let mut services: Vec<String> = moves.iter().flat_map(Move::services).collect();
    services.sort();
    services.dedup();
    orch.stop(&services, opts.timeout).await?;
    let cutover = async {
        for m in &moves {
            let st = rsync(m, opts.xattrs).await?;
            println!(
                "  final pass {}: {} files, {} copied",
                m.to.display(),
                st.files,
                human(st.bytes)
            );
            verify(m, opts.xattrs).await?;
            println!("  verified {}", m.to.display());
        }
        Ok::<(), Error>(())
    };
    if let Err(e) = cutover.await {
        // The old containers still mount the old data: bring them back.
        eprintln!(
            "data move failed; restarting {} on the old data",
            services.join(", ")
        );
        if let Err(e2) = orch.start(&services).await {
            eprintln!("restart failed: {e2}");
        }
        return Err(e);
    }
    for m in &moves {
        if m.from.is_dir() {
            let marker = m.to.join(MARKER);
            std::fs::remove_file(&marker).ctx(|| format!("remove {}", marker.display()))?;
        }
    }
    orch.up_locked(&UpOptions {
        services: services.clone(),
        force_recreate: false,
        pull: None,
        wait_timeout: None,
        wait: false,
        build: false,
    })
    .await?;
    for m in &moves {
        println!(
            "Moved to {}. The old data is still at {}; remove it once the service checks out.",
            m.to.display(),
            m.from.display()
        );
    }
    Ok(())
}

/// Checks the destination and leaves the marker in it.
fn prepare(m: &Move, force: bool) -> Result<()> {
    if !m.from.exists() {
        return Err(Error::State(format!("{} does not exist", m.from.display())));
    }
    if m.to.exists() && !is_empty(&m.to) && !has_marker(&m.to) && !force {
        return Err(Error::State(format!(
            "{} already holds data; data move would replace it. Empty it, or pass --force",
            m.to.display()
        )));
    }
    if let Some(parent) = m.to.parent() {
        std::fs::create_dir_all(parent).ctx(|| format!("create {}", parent.display()))?;
    }
    if m.from.is_dir() {
        std::fs::create_dir_all(&m.to).ctx(|| format!("create {}", m.to.display()))?;
        let marker = m.to.join(MARKER);
        std::fs::write(&marker, format!("moving from {}\n", m.from.display()))
            .ctx(|| format!("write {}", marker.display()))?;
    }
    Ok(())
}

fn which_rsync() -> Result<()> {
    let found = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .any(|d| d.join("rsync").is_file());
    if found {
        Ok(())
    } else {
        Err(Error::State(
            "rsync not found; install it (the gantry package depends on it)".into(),
        ))
    }
}

/// The rsync arguments for one move. Directories copy their contents
/// (trailing slash); file mounts copy the file.
fn rsync_args(m: &Move, xattrs: bool, verify: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![
        if xattrs { "-aHAX" } else { "-aH" }.into(),
        "--numeric-ids".into(),
        "--sparse".into(),
        "--delete".into(),
        format!("--exclude=/{MARKER}"),
    ];
    if verify {
        a.extend([
            "--dry-run".into(),
            "--checksum".into(),
            "--itemize-changes".into(),
        ]);
    } else {
        a.push("--stats".into());
    }
    let (from, to) = if m.from.is_dir() {
        (
            format!("{}/", m.from.display()),
            format!("{}/", m.to.display()),
        )
    } else {
        (m.from.display().to_string(), m.to.display().to_string())
    };
    a.extend(["--".into(), from, to]);
    a
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Stats {
    files: u64,
    bytes: u64,
}

/// One copy pass.
async fn rsync(m: &Move, xattrs: bool) -> Result<Stats> {
    Ok(parse_stats(&rsync_raw(m, xattrs, false).await?))
}

async fn rsync_raw(m: &Move, xattrs: bool, verify: bool) -> Result<String> {
    let args = rsync_args(m, xattrs, verify);
    let mut child = tokio::process::Command::new("rsync")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ctx(|| "run rsync".into())?;
    let mut out = String::new();
    let mut err = String::new();
    let (mut so, mut se) = (
        child
            .stdout
            .take()
            .ok_or_else(|| Error::State("rsync stdout".into()))?,
        child
            .stderr
            .take()
            .ok_or_else(|| Error::State("rsync stderr".into()))?,
    );
    let (r1, r2) = tokio::join!(so.read_to_string(&mut out), se.read_to_string(&mut err));
    r1.ctx(|| "read rsync output".into())?;
    r2.ctx(|| "read rsync errors".into())?;
    let status = child.wait().await.ctx(|| "wait for rsync".into())?;
    if !status.success() {
        let hint = if err.contains("Operation not supported") && xattrs {
            " (the destination may not support ACLs or extended attributes; retry with --no-xattrs)"
        } else {
            ""
        };
        return Err(Error::State(format!(
            "rsync {} -> {} failed ({status}){hint}: {}",
            m.from.display(),
            m.to.display(),
            err.trim()
        )));
    }
    Ok(out)
}

fn parse_stats(out: &str) -> Stats {
    let num = |key: &str| -> u64 {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .replace([',', '.'], "")
            })
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    Stats {
        files: num("Number of regular files transferred:"),
        bytes: num("Total transferred file size:"),
    }
}

/// A checksum dry run must find nothing left to copy.
async fn verify(m: &Move, xattrs: bool) -> Result<()> {
    let out = rsync_raw(m, xattrs, true).await?;
    let diffs: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(Error::State(format!(
            "verification of {} found {} differences, first: {}",
            m.to.display(),
            diffs.len(),
            diffs[..diffs.len().min(5)].join("; ")
        )))
    }
}

/// Lexical cleanup (`a//b/./c/` -> `a/b/c`), without touching the disk.
fn clean(p: &Path) -> PathBuf {
    p.components().collect()
}

fn has_marker(p: &Path) -> bool {
    p.join(MARKER).exists()
}

/// Missing, an empty directory, or an empty file.
fn is_empty(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Err(_) => true,
        Ok(md) if md.is_dir() => std::fs::read_dir(p)
            .map(|mut d| d.all(|e| e.is_ok_and(|e| e.file_name() == MARKER)))
            .unwrap_or(false),
        Ok(md) => md.len() == 0,
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(from: &str, to: &str) -> Move {
        Move {
            from: from.into(),
            to: to.into(),
            volume: None,
            users: vec![("db".into(), "/data".into())],
        }
    }

    #[test]
    fn parses_rsync_stats() {
        let out = "\nNumber of files: 1,204 (reg: 1,100, dir: 104)\nNumber of created files: 3\nNumber of regular files transferred: 1,023\nTotal file size: 9,876,543 bytes\nTotal transferred file size: 1,234,567 bytes\n";
        assert_eq!(
            parse_stats(out),
            Stats {
                files: 1023,
                bytes: 1_234_567
            }
        );
        assert_eq!(parse_stats(""), Stats::default());
    }

    #[test]
    fn rsync_arguments() {
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("old");
        std::fs::create_dir(&from).unwrap();
        let m = mv(from.to_str().unwrap(), "/srv/new");
        let a = rsync_args(&m, true, false);
        assert_eq!(a[0], "-aHAX");
        assert!(a.contains(&"--numeric-ids".to_string()));
        assert!(a.contains(&"--delete".to_string()));
        assert!(a.contains(&format!("--exclude=/{MARKER}")));
        assert_eq!(a[a.len() - 2], format!("{}/", from.display()));
        assert_eq!(a[a.len() - 1], "/srv/new/");
        let v = rsync_args(&m, false, true);
        assert_eq!(v[0], "-aH");
        assert!(v.contains(&"--checksum".to_string()) && v.contains(&"--dry-run".to_string()));
        // A file mount copies the file itself.
        let f = d.path().join("app.conf");
        std::fs::write(&f, "x").unwrap();
        let a = rsync_args(&mv(f.to_str().unwrap(), "/srv/app.conf"), true, false);
        assert_eq!(a[a.len() - 1], "/srv/app.conf");
    }

    #[test]
    fn emptiness_and_marker() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        assert!(is_empty(&p));
        std::fs::create_dir(&p).unwrap();
        assert!(is_empty(&p));
        std::fs::write(p.join(MARKER), "").unwrap();
        assert!(is_empty(&p) && has_marker(&p));
        std::fs::write(p.join("data"), "1").unwrap();
        assert!(!is_empty(&p));
        assert_eq!(clean(Path::new("/a//b/./c/")), PathBuf::from("/a/b/c"));
    }

    #[tokio::test]
    async fn copies_and_verifies() {
        if which_rsync().is_err() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("old");
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("sub/a"), "hello").unwrap();
        std::os::unix::fs::symlink("sub/a", from.join("link")).unwrap();
        std::fs::hard_link(from.join("sub/a"), from.join("hard")).unwrap();
        let to = d.path().join("new");
        let m = mv(from.to_str().unwrap(), to.to_str().unwrap());
        prepare(&m, false).unwrap();
        assert!(has_marker(&to));
        let st = rsync(&m, false).await.unwrap();
        // sub/a is copied once; hard is linked to it, not copied again.
        assert_eq!(st.files, 1);
        verify(&m, false).await.unwrap();
        assert_eq!(
            std::fs::read_link(to.join("link")).unwrap(),
            Path::new("sub/a")
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(to.join("hard")).unwrap().ino(),
            std::fs::metadata(to.join("sub/a")).unwrap().ino()
        );
        // The marker survives --delete, and a second pass copies nothing.
        assert!(has_marker(&to));
        let st = rsync(&m, false).await.unwrap();
        assert_eq!(st.files, 0);
        // A change after the copy fails verification.
        std::fs::write(from.join("sub/b"), "late").unwrap();
        assert!(verify(&m, false).await.is_err());
        // A destination with other data is refused without --force.
        let other = d.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("keep"), "1").unwrap();
        let m2 = mv(from.to_str().unwrap(), other.to_str().unwrap());
        assert!(prepare(&m2, false).is_err());
        assert!(prepare(&m2, true).is_ok());
    }
}
