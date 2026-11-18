//! Starting projects at boot, in pod priority order across the host.
//!
//! `gantry up` on an installed project (one found through
//! `/etc/gantry/project-dirs`) registers it here, and `gantry down`
//! removes it. `gantry-boot.service` runs `gantry boot start` at boot: it
//! brings up every registered project's pods one priority level at a time,
//! lowest first, and waits for each level's services to be running (or
//! healthy) before the next. Pods of equal priority start together. At
//! shutdown `gantry boot stop` goes the other way.

use crate::compose::{LoadOptions, load};
use crate::errors::{Context, Error, Result};
use crate::installed;
use crate::orchestrator::{Orchestrator, UpOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One empty file per registered project, named after it.
pub fn registry_dir() -> PathBuf {
    std::env::var_os("GANTRY_BOOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/gantry/boot"))
}

/// The installed name of a project directory: its directory name, when
/// `gantry --installed NAME` finds exactly this directory. Projects run
/// from anywhere else are not started at boot.
pub fn installed_name(project_dir: &Path) -> Option<String> {
    let name = project_dir.file_name()?.to_str()?.to_string();
    let found = installed::locate(&name).ok()?;
    let same = match (found.canonicalize(), project_dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    same.then_some(name)
}

/// Registers the project in `project_dir` for boot when it is installed.
/// Returns its name when this call added it.
pub fn register(project_dir: &Path) -> Result<Option<String>> {
    let Some(name) = installed_name(project_dir) else {
        return Ok(None);
    };
    let dir = registry_dir();
    std::fs::create_dir_all(&dir).ctx(|| format!("create {}", dir.display()))?;
    let f = dir.join(&name);
    if f.exists() {
        return Ok(None);
    }
    std::fs::write(&f, "").ctx(|| format!("write {}", f.display()))?;
    Ok(Some(name))
}

/// Removes the project in `project_dir` from boot. Returns its name when it
/// was registered.
pub fn unregister(project_dir: &Path) -> Result<Option<String>> {
    let Some(name) = project_dir.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    if !installed::name_ok(name) {
        return Ok(None);
    }
    let f = registry_dir().join(name);
    match std::fs::remove_file(&f) {
        Ok(()) => Ok(Some(name.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).ctx(|| format!("remove {}", f.display())),
    }
}

/// Registered project names, sorted.
pub fn registered() -> Result<Vec<String>> {
    let dir = registry_dir();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).ctx(|| format!("read {}", dir.display())),
    };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| installed::name_ok(n))
        .collect();
    v.sort();
    Ok(v)
}

/// A registered project, loaded.
struct Loaded {
    name: String,
    orch: Orchestrator,
}

fn load_all() -> Vec<Loaded> {
    let mut out = Vec::new();
    for name in registered().unwrap_or_else(|e| {
        tracing::error!("boot: {e}");
        vec![]
    }) {
        let dir = match installed::locate(&name) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{name}: skipped: {e}");
                continue;
            }
        };
        let opts = LoadOptions {
            cwd: Some(dir),
            ..Default::default()
        };
        match load(&opts) {
            Ok(l) if gantry_guard(&name, &l.project) => {
                for w in &l.warnings {
                    tracing::warn!("{name}: {w}");
                }
                out.push(Loaded {
                    name,
                    orch: Orchestrator::new(l.project),
                });
            }
            Ok(_) => {}
            Err(e) => eprintln!("{name}: skipped: {e}"),
        }
    }
    out
}

fn gantry_guard(name: &str, project: &crate::project::Project) -> bool {
    match crate::orchestrator::guard_project_name(project) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{name}: skipped: {e}");
            false
        }
    }
}

/// Priority -> (project index, pod names).
type Levels = BTreeMap<u32, Vec<(usize, Vec<String>)>>;

fn levels(projects: &[Loaded]) -> Levels {
    let mut by: Levels = BTreeMap::new();
    for (i, p) in projects.iter().enumerate() {
        let mut per: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for pod in p.orch.project.pods.values() {
            // Pods with no services have nothing to start.
            if p.orch.project.services_in_pod(&pod.name).next().is_some() {
                per.entry(pod.priority).or_default().push(pod.name.clone());
            }
        }
        for (prio, pods) in per {
            by.entry(prio).or_default().push((i, pods));
        }
    }
    by
}

fn services_of(orch: &Orchestrator, pods: &[String]) -> Vec<String> {
    orch.project
        .services
        .values()
        .filter(|s| pods.contains(&s.pod))
        .map(|s| s.name.clone())
        .collect()
}

/// Brings every registered project up, one pod priority level at a time.
/// A failure in one project is reported and does not stop the others.
/// Returns the number of failures.
pub async fn start(wait_timeout: Duration) -> usize {
    let projects = load_all();
    let mut failures = 0;
    for (prio, entries) in levels(&projects) {
        let names: Vec<String> = entries
            .iter()
            .map(|(i, pods)| format!("{}/{}", projects[*i].name, pods.join(",")))
            .collect();
        println!("priority {prio}: {}", names.join(" "));
        let results = futures_util::future::join_all(entries.iter().map(|(i, pods)| {
            let p = &projects[*i];
            let services = services_of(&p.orch, pods);
            async move {
                let r = p
                    .orch
                    .up(&UpOptions {
                        services,
                        force_recreate: false,
                        pull: None,
                        wait_timeout: Some(wait_timeout),
                        wait: true,
                        build: false,
                    })
                    .await;
                (p.name.clone(), r)
            }
        }))
        .await;
        for (name, r) in results {
            if let Err(e) = r {
                failures += 1;
                eprintln!("{name}: {e}");
            }
        }
    }
    failures
}

/// Stops every registered project in reverse pod priority order, then
/// takes their pods down (releasing DHCP leases). Registrations are kept.
pub async fn stop() -> usize {
    let projects = load_all();
    let mut failures = 0;
    for (prio, entries) in levels(&projects).into_iter().rev() {
        println!("priority {prio}: stopping");
        let results = futures_util::future::join_all(entries.iter().map(|(i, pods)| {
            let p = &projects[*i];
            let services = services_of(&p.orch, pods);
            async move { (p.name.clone(), p.orch.stop(&services, None).await) }
        }))
        .await;
        for (name, r) in results {
            if let Err(e) = r {
                failures += 1;
                eprintln!("{name}: {e}");
            }
        }
    }
    let results = futures_util::future::join_all(
        projects
            .iter()
            .map(|p| async move { (p.name.clone(), p.orch.down(false, None).await) }),
    )
    .await;
    for (name, r) in results {
        if let Err(e) = r {
            failures += 1;
            eprintln!("{name}: {e}");
        }
    }
    failures
}

/// What `gantry boot ls` prints: the order boot would use.
pub fn plan() -> Result<String> {
    let projects = load_all();
    let width = projects
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max("PROJECT".len());
    let mut out = format!("{:<9} {:<width$}  PODS\n", "PRIORITY", "PROJECT");
    for (prio, entries) in levels(&projects) {
        for (i, pods) in entries {
            out.push_str(&format!(
                "{prio:<9} {:<width$}  {}\n",
                projects[i].name,
                pods.join(",")
            ));
        }
    }
    let unloaded: Vec<String> = registered()?
        .into_iter()
        .filter(|n| !projects.iter().any(|p| &p.name == n))
        .collect();
    if !unloaded.is_empty() {
        return Err(Error::config(format!(
            "{out}registered but not loadable: {}",
            unloaded.join(", ")
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let reg = d.path().join("boot");
        let store = d.path().join("store");
        std::fs::create_dir_all(store.join("myapp")).unwrap();
        std::fs::create_dir_all(d.path().join("elsewhere/other")).unwrap();
        let conf = d.path().join("project-dirs");
        std::fs::write(&conf, format!("{}\n", store.display())).unwrap();
        // SAFETY: tests in this module run in one thread at a time for env.
        unsafe {
            std::env::set_var("GANTRY_BOOT_DIR", &reg);
            std::env::set_var("GANTRY_PROJECT_DIRS", &conf);
        }
        assert_eq!(
            installed_name(&store.join("myapp")).as_deref(),
            Some("myapp")
        );
        assert_eq!(installed_name(&d.path().join("elsewhere/other")), None);
        assert_eq!(
            register(&store.join("myapp")).unwrap().as_deref(),
            Some("myapp")
        );
        assert_eq!(register(&store.join("myapp")).unwrap(), None);
        assert_eq!(register(&d.path().join("elsewhere/other")).unwrap(), None);
        assert_eq!(registered().unwrap(), vec!["myapp".to_string()]);
        assert_eq!(
            unregister(&store.join("myapp")).unwrap().as_deref(),
            Some("myapp")
        );
        assert_eq!(unregister(&store.join("myapp")).unwrap(), None);
        assert!(registered().unwrap().is_empty());
    }
}
