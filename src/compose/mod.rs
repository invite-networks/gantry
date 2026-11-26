//! Loading compose files: discovery, `.env`, interpolation, merging of
//! several `-f` files, and normalization into [`crate::project::Project`].

pub mod interpolate;
pub(crate) mod normalize;
pub mod raw;
pub mod units;

pub use normalize::is_dns_name;

use crate::errors::{Context, Error, Result};
use crate::project::{Project, RuntimeKind};
use indexmap::IndexMap;
use serde_norway::{Mapping, Value};
use std::path::{Path, PathBuf};

/// File names tried, in order, when no `-f` is given.
pub const DEFAULT_FILES: &[&str] = &[
    "gantry.yaml",
    "gantry.yml",
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
        within_budget(&text).map_err(|e| Error::config(format!("{}: {e}", file.display())))?;
        let mut doc: Value = serde_norway::from_str(&text)
            .map_err(|e| Error::config(format!("{}: {e}", file.display())))?;
        if doc.is_null() {
            continue;
        }
        merge_fully(&mut doc).map_err(|e| Error::config(format!("{}: {e}", file.display())))?;
        interpolate::interpolate_value(&mut doc, &lookup, &file.display().to_string())?;
        merge_into(&mut merged, doc, "");
    }

    let raw: raw::File =
        serde_norway::from_value(merged).map_err(|e| Error::config(e.to_string()))?;
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
            serde_norway::from_str("services: {web: {image: a, ports: ['80'], command: [x]}}")
                .unwrap();
        let b: Value =
            serde_norway::from_str("services: {web: {image: b, ports: ['443'], command: [y]}}")
                .unwrap();
        merge_into(&mut a, b, "");
        let web = &a["services"]["web"];
        assert_eq!(web["image"].as_str(), Some("b"));
        assert_eq!(web["ports"].as_sequence().unwrap().len(), 2);
        assert_eq!(web["command"].as_sequence().unwrap().len(), 1);
    }
}

/// Most passes of YAML merge keys. A merge source can itself hold `<<`,
/// which one pass copies in as a plain key; real files nest a few levels.
const MAX_MERGE_PASSES: usize = 16;

/// Applies `<<` merge keys until none are left, as Compose does, so a key
/// inside a nested merge source is not silently dropped.
fn merge_fully(doc: &mut Value) -> std::result::Result<(), String> {
    for _ in 0..MAX_MERGE_PASSES {
        doc.apply_merge().map_err(|e| e.to_string())?;
        if !has_merge_key(doc) {
            return Ok(());
        }
    }
    Err(format!(
        "merge keys (<<) nested more than {MAX_MERGE_PASSES} deep"
    ))
}

fn has_merge_key(v: &Value) -> bool {
    match v {
        Value::Mapping(m) => m
            .iter()
            .any(|(k, v)| k.as_str() == Some("<<") || has_merge_key(v)),
        Value::Sequence(s) => s.iter().any(has_merge_key),
        Value::Tagged(t) => has_merge_key(&t.value),
        _ => false,
    }
}

/// Most YAML values a file may hold once its aliases are expanded. A real
/// project has a few thousand; a 250 KB file with thousands of variables
/// stays far below this. Without a cap, a few KB of nested aliases
/// expands to gigabytes while it is parsed.
pub const MAX_NODES: usize = 200_000;

/// Most text (keys and scalars) a file may hold once its aliases are
/// expanded. Counting values alone misses one long string aliased many
/// times.
pub const MAX_TEXT: usize = 4 * 1024 * 1024;

/// Walks the document once, following aliases, storing nothing, and stops
/// as soon as it passes [`MAX_NODES`] values or [`MAX_TEXT`] bytes of text.
pub fn within_budget(text: &str) -> std::result::Result<(), String> {
    use serde::de::DeserializeSeed;
    let budget = Budget::default();
    for doc in serde_norway::Deserializer::from_str(text) {
        Count(&budget).deserialize(doc).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Default)]
struct Budget {
    nodes: std::cell::Cell<usize>,
    text: std::cell::Cell<usize>,
}

struct Count<'a>(&'a Budget);

impl Count<'_> {
    fn bump<E: serde::de::Error>(&self) -> std::result::Result<(), E> {
        self.add(0)
    }

    fn add<E: serde::de::Error>(&self, text: usize) -> std::result::Result<(), E> {
        let n = self.0.nodes.get() + 1;
        self.0.nodes.set(n);
        let t = self.0.text.get().saturating_add(text);
        self.0.text.set(t);
        if n > MAX_NODES {
            Err(E::custom(format!(
                "more than {MAX_NODES} values once aliases are expanded"
            )))
        } else if t > MAX_TEXT {
            Err(E::custom(format!(
                "more than {} MiB of text once aliases are expanded",
                MAX_TEXT / (1024 * 1024)
            )))
        } else {
            Ok(())
        }
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for Count<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Count<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<(), E> {
        self.add(v.len())
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        self.bump()
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        use serde::de::DeserializeSeed;
        self.bump()?;
        Count(self.0).deserialize(d)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<(), A::Error> {
        self.bump()?;
        while seq.next_element_seed(Count(self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<(), A::Error> {
        self.bump()?;
        while map.next_key_seed(Count(self.0))?.is_some() {
            map.next_value_seed(Count(self.0))?;
        }
        Ok(())
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(
        self,
        data: A,
    ) -> std::result::Result<(), A::Error> {
        use serde::de::VariantAccess;
        self.bump()?;
        let ((), v) = data.variant_seed(Count(self.0))?;
        v.newtype_variant_seed(Count(self.0))
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn alias_bombs_are_refused() {
        let n = 4000;
        let mut text = format!("x-big: &big [{}]\nx-refs:\n", vec!["1"; n].join(","));
        for _ in 0..n {
            text.push_str("  - *big\n");
        }
        let started = std::time::Instant::now();
        let e = within_budget(&text).unwrap_err();
        assert!(e.contains("aliases"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn long_aliased_strings_are_refused() {
        let mut text = format!("x-s: &s \"{}\"\nx-refs:\n", "a".repeat(200_000));
        for _ in 0..100 {
            text.push_str("  - *s\n");
        }
        let e = within_budget(&text).unwrap_err();
        assert!(e.contains("MiB of text"), "{e}");
    }

    #[test]
    fn nested_merge_keys_are_applied() {
        let mut doc: Value = serde_norway::from_str(
            "x-i: &i { user: '1000' }\nx-o: &o { <<: *i, working_dir: /srv }\nservices:\n  a: { <<: { <<: *o, image: x }, pod: p }\n",
        )
        .unwrap();
        merge_fully(&mut doc).unwrap();
        let a = &doc["services"]["a"];
        assert_eq!(a["user"], "1000");
        assert_eq!(a["working_dir"], "/srv");
        assert_eq!(a["image"], "x");
        assert!(!has_merge_key(&doc));
    }

    #[test]
    fn ordinary_files_pass() {
        within_budget("name: t\nx-a: &a {image: x}\nservices:\n  a: *a\n  b: { <<: *a, command: [sh] }\n  c: !reset null\n").unwrap();
        within_budget("").unwrap();
    }
}
