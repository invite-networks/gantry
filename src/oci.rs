//! The OCI runtime shim used with Docker. Docker cannot join a namespace
//! by path, so kirisame registers `kirisame-oci` as a Docker runtime. On
//! `create` it reads annotations kirisame put on the container, points the
//! bundle's namespaces at the pod's pinned namespaces, and hands off to the
//! real runtime (runc by default) with the original arguments.

use serde_json::Value;

pub const ANN_NETNS: &str = "io.kirisame.netns";
pub const ANN_UTSNS: &str = "io.kirisame.utsns";
pub const ANN_IPCNS: &str = "io.kirisame.ipcns";
/// Namespace paths must live under these directories, so a container's
/// annotations cannot point the shim at arbitrary files.
pub const ALLOWED_NS_DIRS: &[&str] = &["/run/netns/", "/run/kirisame/", "/var/run/netns/"];

/// The subcommand and bundle directory found in a runtime command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub subcommand: Option<String>,
    pub bundle: Option<String>,
}

/// Global flags of runc and crun that take a value.
const VALUE_FLAGS: &[&str] = &[
    "--root",
    "--log",
    "--log-format",
    "--criu",
    "--rootless",
    "--cgroup-manager",
];

/// Finds the subcommand (`create`, `run`, ...) and `--bundle`/`-b` value.
pub fn parse_invocation(args: &[String]) -> Invocation {
    let mut subcommand = None;
    let mut bundle = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--bundle=") {
            bundle = Some(v.to_string());
        } else if a == "--bundle" || a == "-b" {
            bundle = args.get(i + 1).cloned();
            i += 1;
        } else if subcommand.is_none() && a.starts_with('-') {
            if VALUE_FLAGS.contains(&a.as_str()) {
                i += 1;
            }
        } else if subcommand.is_none() {
            subcommand = Some(a.clone());
        }
        i += 1;
    }
    Invocation { subcommand, bundle }
}

/// Rewrites a bundle's config.json in place. Returns true if anything
/// changed. Errors are strings shown by Docker as the start failure.
pub fn rewrite_spec(spec: &mut Value, ns_exists: &dyn Fn(&str) -> bool) -> Result<bool, String> {
    let annotations = spec
        .get("annotations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let get = |k: &str| {
        annotations
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let targets = [
        ("network", get(ANN_NETNS)),
        ("uts", get(ANN_UTSNS)),
        ("ipc", get(ANN_IPCNS)),
    ];
    if targets.iter().all(|(_, p)| p.is_none()) {
        return Ok(false);
    }
    for (kind, path) in &targets {
        if let Some(p) = path {
            let allowed = ALLOWED_NS_DIRS.iter().any(|d| p.starts_with(d))
                && !p.split('/').any(|c| c == "..");
            if !allowed {
                return Err(format!(
                    "kirisame: {kind} namespace path {p:?} is outside {}",
                    ALLOWED_NS_DIRS.join(", ")
                ));
            }
            if !ns_exists(p) {
                return Err(format!(
                    "kirisame: pod {kind} namespace {p} does not exist; run `kirisame up` again"
                ));
            }
        }
    }
    let linux = spec
        .get_mut("linux")
        .and_then(Value::as_object_mut)
        .ok_or("kirisame: config.json has no linux section")?;
    let namespaces = linux
        .entry("namespaces")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .ok_or("kirisame: linux.namespaces is not a list")?;
    for (kind, path) in &targets {
        let Some(p) = path else { continue };
        match namespaces
            .iter_mut()
            .find(|n| n.get("type").and_then(Value::as_str) == Some(kind))
        {
            Some(n) => {
                n["path"] = Value::String(p.clone());
            }
            None => namespaces.push(serde_json::json!({ "type": kind, "path": p })),
        }
    }
    // Setting a hostname needs a private UTS namespace; the pod's shared
    // one already carries the pod hostname.
    if targets[1].1.is_some() {
        if let Some(obj) = spec.as_object_mut() {
            obj.remove("hostname");
            obj.remove("domainname");
        }
    }
    // Network sysctls would change the shared pod namespace from one
    // container; kirisame sets them on the pod instead.
    if targets[0].1.is_some() {
        if let Some(sysctl) = spec
            .pointer_mut("/linux/sysctl")
            .and_then(Value::as_object_mut)
        {
            sysctl.retain(|k, _| !k.starts_with("net."));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn finds_bundle_and_subcommand() {
        let inv = parse_invocation(&s(&[
            "--root",
            "/r",
            "--log",
            "/l.json",
            "--log-format",
            "json",
            "--systemd-cgroup",
            "create",
            "--bundle",
            "/b",
            "--pid-file",
            "/p",
            "id",
        ]));
        assert_eq!(
            inv,
            Invocation {
                subcommand: Some("create".into()),
                bundle: Some("/b".into())
            }
        );
        let inv = parse_invocation(&s(&["run", "-b=/x", "--bundle=/y", "id"]));
        assert_eq!(inv.bundle.as_deref(), Some("/y"));
        assert_eq!(
            parse_invocation(&s(&["features"])).subcommand.as_deref(),
            Some("features")
        );
    }

    #[test]
    fn rewrites_namespaces() {
        let mut spec = json!({
            "hostname": "abc",
            "annotations": { ANN_NETNS: "/run/netns/p", ANN_UTSNS: "/run/kirisame/x/uts" },
            "linux": {
                "namespaces": [{"type": "mount"}, {"type": "network"}, {"type": "uts"}, {"type": "ipc"}],
                "sysctl": {"net.ipv4.ip_forward": "1", "kernel.msgmax": "1"}
            }
        });
        assert!(rewrite_spec(&mut spec, &|_| true).unwrap());
        let ns = spec["linux"]["namespaces"].as_array().unwrap();
        assert_eq!(ns[1]["path"], "/run/netns/p");
        assert_eq!(ns[2]["path"], "/run/kirisame/x/uts");
        assert!(ns[3].get("path").is_none());
        assert!(spec.get("hostname").is_none());
        assert!(spec["linux"]["sysctl"].get("net.ipv4.ip_forward").is_none());
        assert!(spec["linux"]["sysctl"].get("kernel.msgmax").is_some());
    }

    #[test]
    fn untouched_without_annotations_and_errors_on_missing_ns() {
        let mut spec = json!({"linux": {"namespaces": []}});
        assert!(!rewrite_spec(&mut spec, &|_| true).unwrap());
        for bad in ["/etc/shadow", "/run/netns/../../etc/x"] {
            let mut spec = json!({"annotations": {ANN_NETNS: bad}, "linux": {"namespaces": []}});
            assert!(
                rewrite_spec(&mut spec, &|_| true)
                    .unwrap_err()
                    .contains("outside")
            );
        }
        let mut spec =
            json!({"annotations": {ANN_NETNS: "/run/netns/gone"}, "linux": {"namespaces": []}});
        assert!(
            rewrite_spec(&mut spec, &|_| false)
                .unwrap_err()
                .contains("does not exist")
        );
    }
}
