//! Installed projects: found by name in the project directories listed in
//! `/etc/gantry/project-dirs`, so a project can live next to its data (for
//! example `/mnt/container-2/myapp/`) and still be started by
//! `gantry@myapp.service` or over remote control.

use crate::errors::{Context, Error, Result};
use std::path::{Path, PathBuf};

/// One parent directory pattern per line. `*` and `?` match within one
/// path component. Blank lines and `#` comments are ignored.
pub const CONFIG: &str = "/etc/gantry/project-dirs";

/// Used when the config file does not exist.
pub const DEFAULT_DIRS: &[&str] = &["/etc/gantry"];

/// Names that are never projects (`/etc/gantry/remote` holds keys).
const RESERVED: &[&str] = &["remote", "project-dirs"];

/// The config path, overridable for tests with `GANTRY_PROJECT_DIRS`.
pub fn config_path() -> PathBuf {
    std::env::var_os("GANTRY_PROJECT_DIRS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(CONFIG))
}

/// The parent directory patterns from the config, or the defaults.
pub fn patterns(config: &Path) -> Result<Vec<String>> {
    let text = match std::fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DEFAULT_DIRS.iter().map(|s| (*s).to_string()).collect());
        }
        Err(e) => return Err(e).ctx(|| format!("read {}", config.display())),
    };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with('/') {
            return Err(Error::config(format!(
                "{}:{}: {line:?} is not an absolute path",
                config.display(),
                i + 1
            )));
        }
        out.push(line.to_string());
    }
    Ok(out)
}

/// A project name: letters, digits, `.`, `_`, `-`, not starting with `.`
/// or `-`.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !RESERVED.contains(&name)
}

/// Finds the installed project `name`. It must exist in exactly one of the
/// project directories, as a real directory (not a symlink).
pub fn locate(name: &str) -> Result<PathBuf> {
    locate_in(name, &patterns(&config_path())?)
}

pub fn locate_in(name: &str, patterns: &[String]) -> Result<PathBuf> {
    if !name_ok(name) {
        return Err(Error::config(format!("invalid project name {name:?}")));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for pat in patterns {
        for parent in expand(pat) {
            let p = parent.join(name);
            let is_dir = std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
            if is_dir && !found.contains(&p) {
                found.push(p);
            }
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::config(format!(
            "no installed project {name:?} in {}",
            patterns.join(", ")
        ))),
        many => Err(Error::config(format!(
            "project {name:?} is installed more than once: {}; keep one",
            many.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Expands `*` and `?` in each component of an absolute pattern. Only real
/// directories match; symlinks are skipped so a pattern cannot be pointed
/// elsewhere.
fn expand(pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![PathBuf::from("/")];
    for comp in pattern.split('/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &current {
            if comp.contains(['*', '?']) {
                let Ok(rd) = std::fs::read_dir(base) else {
                    continue;
                };
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.') && wildcard(comp, n))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| base.join(n)));
            } else {
                let p = base.join(comp);
                if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                    next.push(p);
                }
            }
        }
        current = next;
    }
    current
}

/// `*` matches any run of characters, `?` one character.
fn wildcard(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                mark = ti;
                pi += 1;
            }
            Some(c) if *c == '?' || *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some(sp) => {
                    pi = sp + 1;
                    mark += 1;
                    ti = mark;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "container-2"));
        assert!(wildcard("container-*", "container-local"));
        assert!(wildcard("container-?", "container-2"));
        assert!(!wildcard("container-?", "container-local"));
        assert!(!wildcard("container-*", "data"));
        assert!(wildcard("a*b*c", "aXXbYc"));
    }

    #[test]
    fn names() {
        assert!(name_ok("myapp") && name_ok("my_app.v2"));
        for bad in ["", ".x", "-x", "a/b", "..", "remote", "a b"] {
            assert!(!name_ok(bad), "{bad}");
        }
    }

    #[test]
    fn finds_one_refuses_two() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        for p in [
            "etc",
            "mnt/container-local/db1",
            "mnt/container-2/web",
            "mnt/container-2/both",
            "mnt/container-local/both",
        ] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("mnt/container-2/web"), root.join("etc/linked"))
            .unwrap();
        let pats = vec![
            format!("{}/etc", root.display()),
            format!("{}/mnt/*", root.display()),
        ];
        assert_eq!(
            locate_in("web", &pats).unwrap(),
            root.join("mnt/container-2/web")
        );
        assert_eq!(
            locate_in("db1", &pats).unwrap(),
            root.join("mnt/container-local/db1")
        );
        let e = locate_in("both", &pats).unwrap_err().to_string();
        assert!(e.contains("more than once"), "{e}");
        let e = locate_in("nope", &pats).unwrap_err().to_string();
        assert!(e.contains("no installed project"), "{e}");
        // Symlinked projects are not followed.
        assert!(locate_in("linked", &pats).is_err());
        assert!(locate_in("../etc", &pats).is_err());
    }

    #[test]
    fn config_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("project-dirs");
        assert_eq!(patterns(&f).unwrap(), vec!["/etc/gantry".to_string()]);
        std::fs::write(&f, "# where projects live\n/etc/gantry\n\n/mnt/*\n").unwrap();
        assert_eq!(patterns(&f).unwrap(), vec!["/etc/gantry", "/mnt/*"]);
        std::fs::write(&f, "mnt/*\n").unwrap();
        assert!(patterns(&f).is_err());
    }
}
