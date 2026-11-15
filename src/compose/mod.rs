//! Loading compose files: discovery, `.env`, interpolation, merging of
//! several `-f` files, and normalization into [`crate::project::Project`].

pub mod interpolate;
mod normalize;
pub mod raw;
pub mod units;

use crate::errors::{Context, Error, Result};
use crate::project::{Project, RuntimeKind};
use indexmap::IndexMap;
use serde_yaml_ng::{Mapping, Value};
use std::path::{Path, PathBuf};

/// File names tried, in order, when no `-f` is given.
pub const DEFAULT_FILES: &[&str] = &[
    "kirisame.yaml",
    "kirisame.yml",
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

#[derive(Debug, Default, Clone)]
pub struct LoadOptions {
    pub files: Vec<PathBuf>,
    pub project_name: Option<String>,
    pub project_dir: Option<PathBuf>,
    pub env_files: Vec<PathBuf>,
    pub profiles: Vec<String>,
    /// Forces every service onto this runtime.
    pub runtime: Option<RuntimeKind>,
    /// Working directory used for discovery. Defaults to the process cwd.
    pub cwd: Option<PathBuf>,
}

/// A loaded project plus any warnings worth showing the user.
#[derive(Debug)]
pub struct Loaded {
    pub project: Project,
    pub warnings: Vec<String>,
    pub files: Vec<PathBuf>,
}

pub fn load(opts: &LoadOptions) -> Result<Loaded> {
    let cwd = match &opts.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().ctx(|| "current directory".into())?,
    };
    let files = if opts.files.is_empty() {
        discover(&cwd)?
    } else {
        opts.files.iter().map(|f| cwd.join(f)).collect()
    };
    let project_dir = match &opts.project_dir {
        Some(d) => cwd.join(d),
        None => files[0]
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.clone()),
    };

    let mut dotenv = IndexMap::new();
    let env_files: Vec<PathBuf> = if opts.env_files.is_empty() {
        let default = project_dir.join(".env");
        if default.is_file() {
            vec![default]
        } else {
            vec![]
        }
    } else {
        opts.env_files.iter().map(|f| cwd.join(f)).collect()
    };
    for f in &env_files {
        dotenv.extend(read_env_file(f)?);
    }
    let lookup = move |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| dotenv.get(name).cloned())
    };

    let mut merged = Value::Mapping(Mapping::new());
    for file in &files {
        let text = std::fs::read_to_string(file).ctx(|| format!("read {}", file.display()))?;
        let mut doc: Value = serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::config(format!("{}: {e}", file.display())))?;
        if doc.is_null() {
            continue;
        }
        doc.apply_merge()
            .map_err(|e| Error::config(format!("{}: {e}", file.display())))?;
        interpolate::interpolate_value(&mut doc, &lookup, &file.display().to_string())?;
        merge_into(&mut merged, doc, "");
    }

    let raw: raw::File =
        serde_yaml_ng::from_value(merged).map_err(|e| Error::config(e.to_string()))?;
    let mut profiles = opts.profiles.clone();
    if profiles.is_empty() {
        if let Some(p) = lookup("COMPOSE_PROFILES") {
            profiles = p
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    let ctx = normalize::Ctx {
        project_dir: project_dir.clone(),
        project_name: opts.project_name.clone(),
        runtime_override: opts.runtime,
        profiles,
        lookup: &lookup,
    };
    let (project, warnings) = normalize::normalize(raw, &ctx)?;
    Ok(Loaded {
        project,
        warnings,
        files,
    })
}

fn discover(cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for name in DEFAULT_FILES {
            let p = d.join(name);
            if p.is_file() {
                let mut files = vec![p.clone()];
                let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
                for ext in ["yaml", "yml"] {
                    let ov = d.join(format!("{stem}.override.{ext}"));
                    if ov.is_file() {
                        files.push(ov);
                        break;
                    }
                }
                return Ok(files);
            }
        }
        dir = d.parent();
    }
    Err(Error::config(format!(
        "no compose file found in {} or its parents (looked for {})",
        cwd.display(),
        DEFAULT_FILES.join(", ")
    )))
}

/// Sequences under these keys are concatenated when files are merged;
/// everything else is replaced, and mappings merge recursively.
const APPEND_KEYS: &[&str] = &[
    "ports",
    "expose",
    "volumes",
    "devices",
    "cap_add",
    "cap_drop",
    "dns",
    "dns_search",
    "tmpfs",
    "env_file",
    "profiles",
    "security_opt",
    "group_add",
    "input",
    "output",
];

pub(crate) fn merge_into(base: &mut Value, overlay: Value, key: &str) {
    match (base, overlay) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                let child_key = k.as_str().unwrap_or("").to_string();
                match b.get_mut(&k) {
                    Some(existing) => merge_into(existing, v, &child_key),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (Value::Sequence(b), Value::Sequence(o)) if APPEND_KEYS.contains(&key) => b.extend(o),
        (b, o) => *b = o,
    }
}

/// Reads a `KEY=VALUE` env file (comments, blank lines, `export`, quotes).
pub fn read_env_file(path: &Path) -> Result<IndexMap<String, String>> {
    let text = std::fs::read_to_string(path).ctx(|| format!("read env file {}", path.display()))?;
    parse_env(&text).map_err(|e| Error::config(format!("{}: {e}", path.display())))
}

pub fn parse_env(text: &str) -> std::result::Result<IndexMap<String, String>, String> {
    let mut out = IndexMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=VALUE", n + 1));
        };
        let k = k.trim();
        if k.is_empty()
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            return Err(format!("line {}: invalid variable name {k:?}", n + 1));
        }
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            let inner = &v[1..v.len() - 1];
            if v.starts_with('"') {
                inner.replace("\\n", "\n").replace("\\\"", "\"")
            } else {
                inner.to_string()
            }
        } else {
            // Strip an inline comment that follows whitespace.
            match v.find(" #") {
                Some(i) => v[..i].trim_end().to_string(),
                None => v.to_string(),
            }
        };
        out.insert(k.to_string(), v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let env =
            parse_env("# c\nA=1\nexport B = two \nC=\"x y\"\nD='$raw'\nE=v # note\n\n").unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two");
        assert_eq!(env["C"], "x y");
        assert_eq!(env["D"], "$raw");
        assert_eq!(env["E"], "v");
        assert!(parse_env("novalue").is_err());
    }

    #[test]
    fn merge_rules() {
        let mut a: Value =
            serde_yaml_ng::from_str("services: {web: {image: a, ports: ['80'], command: [x]}}")
                .unwrap();
        let b: Value =
            serde_yaml_ng::from_str("services: {web: {image: b, ports: ['443'], command: [y]}}")
                .unwrap();
        merge_into(&mut a, b, "");
        let web = &a["services"]["web"];
        assert_eq!(web["image"].as_str(), Some("b"));
        assert_eq!(web["ports"].as_sequence().unwrap().len(), 2);
        assert_eq!(web["command"].as_sequence().unwrap().len(), 1);
    }
}
