//! `check`: validates compose text on its own, with no filesystem and no
//! process environment, for a caller such as a server that holds the
//! text but not the host it will run on.
//!
//! It runs the same parser and validation as `load`, with three changes:
//! variables come only from the caller's lookup, services' `env_file`
//! entries are listed but not read, and `~/` paths are left as written.
//! A variable the lookup cannot resolve is substituted as Compose would
//! (empty, or the default), a required `${X:?}` included, so a value that
//! depends on it may fail validation. The lists of variables, network
//! parents and env files do not depend on the lookup and are filled in even
//! when the file has issues.

use super::interpolate::{self, key_str};
use super::normalize::{self, Home};
use super::{merge_fully, raw};
use serde::Serialize;
use serde_norway::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Which profiles' services to check.
#[derive(Debug, Clone, Default)]
pub enum CheckProfiles {
    /// Every service, whatever its profiles.
    #[default]
    All,
    /// Services without profiles, plus those in these.
    Only(Vec<String>),
}

pub struct CheckOptions<'a> {
    /// Overrides `name:` in the file.
    pub project_name: Option<String>,
    pub profiles: CheckProfiles,
    /// Values for `${VAR}`. A lookup that resolves nothing checks the file
    /// as written.
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
    /// Check as `LoadOptions::isolated_env` loads: each `~/` path in a
    /// volume source, build context or `env_file` is an issue.
    pub isolated_env: bool,
}

/// One problem with the text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub message: String,
    /// Where in the document, such as `services.web.image`, when known.
    pub path: Option<String>,
    /// For YAML syntax errors and limits.
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// The result of [`check`]: its issues, and what the file needs from the
/// host it runs on. The lists are filled in as far as the text could be
/// parsed, issues or not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Checked {
    /// Empty when the file is valid.
    pub issues: Vec<Issue>,
    /// The project name, when the file got as far as validation.
    pub project: Option<String>,
    /// Variables used without a default somewhere (`$X`, `${X}`,
    /// `${X:?}`): a host must supply these.
    pub variables_without_default: Vec<String>,
    /// Variables that always have a default (`${X:-d}`, `${X:+d}`).
    pub variables_with_default: Vec<String>,
    /// Host interfaces the pods attach to (`pod.network` and
    /// `pods.*.network`). An entry that uses an unresolved variable is
    /// given as written.
    pub network_parents: Vec<String>,
    /// Services' `env_file` entries, which a host reads and this does not.
    pub env_files: Vec<String>,
    pub warnings: Vec<String>,
}

fn issue(message: impl Into<String>, path: Option<String>) -> Issue {
    Issue {
        message: message.into(),
        path,
        line: None,
        column: None,
    }
}

fn yaml_issue(e: &serde_norway::Error) -> Issue {
    let loc = e.location();
    // The location is carried in the fields; keep the message plain.
    let mut message = e.to_string();
    if let Some(i) = message.rfind(" at line ") {
        message.truncate(i);
    }
    Issue {
        message,
        path: None,
        line: loc.as_ref().map(|l| l.line() as u32),
        column: loc.as_ref().map(|l| l.column() as u32),
    }
}

pub fn check(text: &str, opts: &CheckOptions<'_>) -> Checked {
    let mut checked = Checked::default();
    if let Err(issues) = run(text, opts, &mut checked) {
        checked.issues = issues;
    }
    checked
}

fn run(text: &str, opts: &CheckOptions<'_>, checked: &mut Checked) -> Result<(), Vec<Issue>> {
    super::budget(text).map_err(|e| vec![yaml_issue(&e)])?;
    let mut doc: Value = serde_norway::from_str(text).map_err(|e| vec![yaml_issue(&e)])?;
    if doc.is_null() {
        return Err(vec![issue("the file is empty", None)]);
    }
    merge_fully(&mut doc).map_err(|e| vec![issue(e, None)])?;

    let lookup = opts.lookup;

    // Variables, from the merged text before substitution.
    let mut needed: BTreeMap<String, bool> = BTreeMap::new();
    let mut issues = Vec::new();
    collect_references(&doc, "", &mut needed, &mut issues);
    if !issues.is_empty() {
        return Err(issues);
    }
    for (name, without_default) in &needed {
        if *without_default {
            checked.variables_without_default.push(name.clone());
        } else {
            checked.variables_with_default.push(name.clone());
        }
    }
    // A value is shown as written when it needs a variable the lookup lacks.
    let unresolved = |s: &str| {
        interpolate::references(s).is_ok_and(|r| {
            r.iter()
                .any(|r| !r.has_default && lookup(&r.name).is_none())
        })
    };

    // Network parents and env files, seeing through tags on keys.
    for net in pod_networks(&doc) {
        let Some(parent) = net.as_mapping().and_then(|m| get(m, "parent")) else {
            continue;
        };
        let Some(raw_parent) = parent.as_str() else {
            continue;
        };
        let value = if unresolved(raw_parent) {
            raw_parent.to_string()
        } else {
            interpolate::interpolate_lenient(raw_parent, &lookup).unwrap_or_default()
        };
        if !value.is_empty() && !checked.network_parents.contains(&value) {
            checked.network_parents.push(value);
        }
    }
    if let Some(services) = doc
        .as_mapping()
        .and_then(|m| get(m, "services"))
        .and_then(Value::as_mapping)
    {
        for (_, svc) in services {
            let Some(env_file) = svc.as_mapping().and_then(|m| get(m, "env_file")) else {
                continue;
            };
            for f in env_file_paths(env_file) {
                if !checked.env_files.contains(&f) {
                    checked.env_files.push(f);
                }
            }
        }
    }

    interpolate::interpolate_value_lenient(&mut doc, &lookup, "")
        .map_err(|(path, e)| vec![issue(e, Some(path))])?;

    let home_issues = if opts.isolated_env {
        home_paths(&doc)
    } else {
        vec![]
    };

    let profiles = match &opts.profiles {
        CheckProfiles::Only(p) => p.clone(),
        CheckProfiles::All => all_profiles(&doc),
    };
    let file: raw::File = serde_norway::from_value(doc).map_err(|e| vec![yaml_issue(&e)])?;
    let ctx = normalize::Ctx {
        // Relative paths resolve under a placeholder; nothing is read.
        project_dir: PathBuf::from("/project"),
        project_name: opts.project_name.clone(),
        runtime_override: None,
        profiles,
        lookup: &lookup,
        read_env_files: false,
        home: Home::Keep,
    };
    let (project, warnings) = match normalize::normalize(file, &ctx) {
        Ok(v) => v,
        Err(e) => {
            let mut all = home_issues;
            all.push(issue(e.to_string(), None));
            return Err(all);
        }
    };
    checked.project = Some(project.name);
    checked.warnings = warnings;
    if home_issues.is_empty() {
        Ok(())
    } else {
        Err(home_issues)
    }
}

/// Each `~/` path an isolated load refuses, after substitution.
fn home_paths(doc: &Value) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut flag = |p: &str, path: String| {
        if p.starts_with("~/") {
            out.push(issue(normalize::home_refused(p), Some(path)));
        }
    };
    let services = doc
        .as_mapping()
        .and_then(|m| get(m, "services"))
        .and_then(Value::as_mapping);
    for (name, svc) in services.into_iter().flatten() {
        let name = key_str(name).unwrap_or("?");
        let Some(svc) = svc.as_mapping() else {
            continue;
        };
        let volumes = get(svc, "volumes").and_then(Value::as_sequence);
        for (i, v) in volumes.into_iter().flatten().map(untag).enumerate() {
            let at = format!("services.{name}.volumes[{i}]");
            match v {
                Value::String(s) => flag(s.split(':').next().unwrap_or(""), at),
                Value::Mapping(m) => {
                    if let Some(src) = get(m, "source").and_then(Value::as_str) {
                        flag(src, format!("{at}.source"));
                    }
                }
                _ => {}
            }
        }
        match get(svc, "build") {
            Some(Value::String(c)) => flag(c, format!("services.{name}.build")),
            Some(Value::Mapping(b)) => {
                if let Some(c) = get(b, "context").and_then(Value::as_str) {
                    flag(c, format!("services.{name}.build.context"));
                }
            }
            _ => {}
        }
        if let Some(env_file) = get(svc, "env_file") {
            for (i, f) in env_file_paths(env_file).iter().enumerate() {
                let at = if env_file.is_string() {
                    format!("services.{name}.env_file")
                } else {
                    format!("services.{name}.env_file[{i}]")
                };
                flag(f, at);
            }
        }
    }
    out
}

/// A value in a mapping by key, seeing through a tag on the key.
fn get<'v>(m: &'v serde_norway::Mapping, key: &str) -> Option<&'v Value> {
    m.iter()
        .find(|(k, _)| key_str(k) == Some(key))
        .map(|(_, v)| untag(v))
}

fn untag(v: &Value) -> &Value {
    match v {
        Value::Tagged(t) => untag(&t.value),
        other => other,
    }
}

/// Every network entry of `pod:` and of each pod under `pods:`.
fn pod_networks(doc: &Value) -> Vec<&Value> {
    let Some(top) = doc.as_mapping() else {
        return vec![];
    };
    let mut pods: Vec<&Value> = Vec::new();
    if let Some(p) = get(top, "pod") {
        pods.push(p);
    }
    if let Some(m) = get(top, "pods").and_then(Value::as_mapping) {
        pods.extend(m.iter().map(|(_, v)| untag(v)));
    }
    pods.into_iter()
        .filter_map(|p| p.as_mapping().and_then(|m| get(m, "network")))
        .filter_map(Value::as_sequence)
        .flat_map(|s| s.iter().map(untag))
        .collect()
}

fn env_file_paths(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Sequence(items) => items
            .iter()
            .map(untag)
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Mapping(m) => get(m, "path").and_then(Value::as_str).map(String::from),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Every profile any service names, so `All` activates them all.
fn all_profiles(doc: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let services = doc
        .as_mapping()
        .and_then(|m| get(m, "services"))
        .and_then(Value::as_mapping);
    for (_, svc) in services.into_iter().flatten() {
        let profiles = svc
            .as_mapping()
            .and_then(|m| get(m, "profiles"))
            .and_then(Value::as_sequence);
        for p in profiles.into_iter().flatten().filter_map(Value::as_str) {
            if !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Records every variable in string values (not keys), with whether any
/// use of it lacks a default.
fn collect_references(
    v: &Value,
    path: &str,
    needed: &mut BTreeMap<String, bool>,
    issues: &mut Vec<Issue>,
) {
    match v {
        Value::String(s) if s.contains('$') => match interpolate::references(s) {
            Ok(refs) => {
                for r in refs {
                    let e = needed.entry(r.name).or_insert(false);
                    *e |= !r.has_default;
                }
            }
            Err(e) => issues.push(issue(e, Some(path.to_string()))),
        },
        Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_references(item, &format!("{path}[{i}]"), needed, issues);
            }
        }
        Value::Mapping(m) => {
            for (k, item) in m {
                let key = key_str(k).unwrap_or("?");
                let p = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                collect_references(item, &p, needed, issues);
            }
        }
        Value::Tagged(t) => collect_references(&t.value, path, needed, issues),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(text: &str) -> Checked {
        let none = |_: &str| None;
        check(
            text,
            &CheckOptions {
                project_name: None,
                profiles: CheckProfiles::All,
                lookup: &none,
                isolated_env: false,
            },
        )
    }

    const FILE: &str = r#"name: shop
pods:
  web:
    network:
      - { parent: "${LAN_PARENT}", address: "${WEB_IP:?set it}" }
      - { parent: vlan30, address: dhcp }
services:
  app:
    image: "registry/app:${TAG:-1.0}"
    pod: web
    env_file: [/etc/shadow, { path: ./app.env, required: false }]
    environment: [PASSTHROUGH]
    volumes: ["~/data:/data"]
  debug:
    image: busybox
    pod: web
    profiles: [debug]
"#;

    /// Values a server would supply while it has no real ones.
    fn stand_ins(k: &str) -> Option<String> {
        match k {
            "LAN_PARENT" => Some("eth9".into()),
            "WEB_IP" => Some("192.0.2.10/24".into()),
            _ => None,
        }
    }

    #[test]
    fn lists_what_the_host_must_supply() {
        // Without values the address check fails, but the lists are there.
        let c = none(FILE);
        assert!(c.issues[0].message.contains("parent"), "{:?}", c.issues);
        assert_eq!(c.variables_without_default, ["LAN_PARENT", "WEB_IP"]);
        assert_eq!(c.variables_with_default, ["TAG"]);
        assert_eq!(c.network_parents, ["${LAN_PARENT}", "vlan30"]);
        assert_eq!(c.env_files, ["/etc/shadow", "./app.env"]);

        let c = check(
            FILE,
            &CheckOptions {
                project_name: None,
                profiles: CheckProfiles::All,
                lookup: &stand_ins,
                isolated_env: false,
            },
        );
        assert!(c.issues.is_empty(), "{:?}", c.issues);
        assert_eq!(c.project.as_deref(), Some("shop"));
        assert_eq!(c.variables_without_default, ["LAN_PARENT", "WEB_IP"]);
        assert_eq!(c.network_parents, ["eth9", "vlan30"]);
    }

    #[test]
    fn isolated_matches_isolated_load() {
        let file = "name: h\nservices:\n  a:\n    image: x\n    network_mode: none\n    volumes: [\"~/data:/data\", { type: bind, source: \"${SRC:-~/s}\", target: /s }]\n    build: { context: ~/ctx }\n    env_file: ~/a.env\n";
        let opts = |isolated_env| CheckOptions {
            project_name: None,
            profiles: CheckProfiles::All,
            lookup: &|_: &str| None,
            isolated_env,
        };
        let c = check(file, &opts(true));
        let paths: Vec<_> = c
            .issues
            .iter()
            .map(|i| i.path.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            paths,
            [
                "services.a.volumes[0]",
                "services.a.volumes[1].source",
                "services.a.build.context",
                "services.a.env_file"
            ]
        );
        assert!(c.issues[0].message.contains("~/ paths are not allowed"));
        assert!(check(file, &opts(false)).issues.is_empty());

        // The isolated load refuses the same file with the same words.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), file).unwrap();
        let e = crate::compose::load(&crate::compose::LoadOptions {
            cwd: Some(dir.path().to_path_buf()),
            isolated_env: true,
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("~/ paths are not allowed"), "{e}");
    }

    #[test]
    fn resolved_variables_are_checked() {
        let vals = |k: &str| match k {
            "LAN_PARENT" => Some("vlan4".to_string()),
            "WEB_IP" => Some("192.0.2.300/24".to_string()),
            _ => None,
        };
        let issues = check(
            FILE,
            &CheckOptions {
                project_name: Some("other".into()),
                profiles: CheckProfiles::Only(vec![]),
                lookup: &vals,
                isolated_env: false,
            },
        )
        .issues;
        assert!(issues[0].message.contains("192.0.2.300"), "{issues:?}");
    }

    #[test]
    fn syntax_errors_have_a_location() {
        let issues = none("name: x\nservices:\n  a: [unclosed\n").issues;
        assert!(
            issues[0].line.is_some() && issues[0].column.is_some(),
            "{issues:?}"
        );
        let issues = none("services:\n  a:\n    image: \"${A:-${B:-${C\"\n").issues;
        assert_eq!(
            issues[0].path.as_deref(),
            Some("services.a.image"),
            "{issues:?}"
        );
    }

    #[test]
    fn tags_and_nested_merges_do_not_hide_anything() {
        let c = none(
            r#"name: t
x-net: &net { network: [ { parent: hidden0, address: dhcp } ] }
!x pods:
  !y web: { <<: { <<: *net } }
!z services:
  a: { image: x, pod: web, !w env_file: /etc/secret }
"#,
        );
        assert!(c.issues.is_empty(), "{:?}", c.issues);
        assert_eq!(c.network_parents, ["hidden0"]);
        assert_eq!(c.env_files, ["/etc/secret"]);
    }

    #[test]
    fn limits_apply() {
        let mut text = format!("x-big: &big [{}]\nx-refs:\n", vec!["1"; 1000].join(","));
        for _ in 0..1000 {
            text.push_str("  - *big\n");
        }
        let issues = none(&text).issues;
        assert!(issues[0].message.contains("aliases"), "{issues:?}");
        assert!(issues[0].line.is_some());
    }
}
