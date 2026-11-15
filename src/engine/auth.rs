//! Registry credentials for pulls. gantry reads the same auth files the
//! engine CLIs write (`docker login`, `podman login`) and supports
//! credential helpers, so private images pull without extra setup.

use crate::project::RuntimeKind;
use base64::Engine as _;
use bollard::auth::DockerCredentials;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Splits an image reference into the repository and the tag or digest
/// (`latest` when neither is given).
pub fn split_reference(image: &str) -> (String, String) {
    if let Some((name, digest)) = image.split_once('@') {
        return (name.to_string(), digest.to_string());
    }
    let slash = image.rfind('/').map_or(0, |i| i + 1);
    match image[slash..].rfind(':') {
        Some(i) => (
            image[..slash + i].to_string(),
            image[slash + i + 1..].to_string(),
        ),
        None => (image.to_string(), "latest".to_string()),
    }
}

/// The registry host an image comes from; `docker.io` for bare names.
pub fn registry_of(name: &str) -> String {
    match name.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first.to_string()
        }
        _ => "docker.io".to_string(),
    }
}

/// Normalizes an auth file key (`https://index.docker.io/v1/`,
/// `registry.example.com`, ...) to a host for comparison.
fn key_host(key: &str) -> String {
    let k = key
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = k.split('/').next().unwrap_or(k);
    match host {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            "docker.io".into()
        }
        h => h.to_string(),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// Auth files in the order the runtime's own CLI would read them.
fn auth_files(kind: RuntimeKind) -> Vec<PathBuf> {
    let docker = std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".docker"))
        .join("config.json");
    match kind {
        RuntimeKind::Podman => {
            let mut v = Vec::new();
            if let Some(f) = std::env::var_os("REGISTRY_AUTH_FILE") {
                v.push(PathBuf::from(f));
            }
            let uid = nix::unistd::getuid().as_raw();
            v.push(PathBuf::from(format!("/run/containers/{uid}/auth.json")));
            v.push(home().join(".config/containers/auth.json"));
            v.push(docker);
            v
        }
        _ => vec![docker],
    }
}

/// Credentials for the registry `name` is pulled from, if any auth file
/// has them.
pub fn credentials_for(kind: RuntimeKind, name: &str) -> Option<DockerCredentials> {
    let registry = registry_of(name);
    for file in auth_files(kind) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(cfg) = serde_json::from_str::<Value>(&text) else {
            tracing::warn!("{}: not valid JSON, ignored", file.display());
            continue;
        };
        if let Some(c) = from_config(&cfg, &registry) {
            return Some(c);
        }
    }
    None
}

fn from_config(cfg: &Value, registry: &str) -> Option<DockerCredentials> {
    // A per-registry helper wins, then the static entry, then the default
    // credential store.
    if let Some(helper) = cfg
        .get("credHelpers")
        .and_then(Value::as_object)
        .and_then(|m| m.iter().find(|(k, _)| key_host(k) == registry))
        .and_then(|(_, v)| v.as_str())
    {
        return from_helper(helper, registry);
    }
    if let Some((key, entry)) = cfg
        .get("auths")
        .and_then(Value::as_object)
        .and_then(|m| m.iter().find(|(k, _)| key_host(k) == registry))
    {
        if let Some(c) = from_entry(key, entry) {
            return Some(c);
        }
    }
    cfg.get("credsStore")
        .and_then(Value::as_str)
        .and_then(|h| from_helper(h, registry))
}

fn from_entry(key: &str, entry: &Value) -> Option<DockerCredentials> {
    let s = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_string);
    if let Some(token) = s("identitytoken") {
        return Some(DockerCredentials {
            identitytoken: Some(token),
            serveraddress: Some(key.to_string()),
            ..Default::default()
        });
    }
    let (user, pass) = match (s("username"), s("password")) {
        (Some(u), Some(p)) => (u, p),
        _ => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(s("auth")?)
                .ok()?;
            let text = String::from_utf8(raw).ok()?;
            let (u, p) = text.split_once(':')?;
            (u.to_string(), p.to_string())
        }
    };
    Some(DockerCredentials {
        username: Some(user),
        password: Some(pass),
        serveraddress: Some(key.to_string()),
        ..Default::default()
    })
}

/// Asks `docker-credential-<helper>` for a registry's credentials.
fn from_helper(helper: &str, registry: &str) -> Option<DockerCredentials> {
    if !helper
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        tracing::warn!("ignoring credential helper with an unexpected name: {helper:?}");
        return None;
    }
    let server = if registry == "docker.io" {
        "https://index.docker.io/v1/".to_string()
    } else {
        registry.to_string()
    };
    let mut child = Command::new(format!("docker-credential-{helper}"))
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(server.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let user = v.get("Username")?.as_str()?.to_string();
    let secret = v.get("Secret")?.as_str()?.to_string();
    Some(if user == "<token>" {
        DockerCredentials {
            identitytoken: Some(secret),
            serveraddress: Some(server),
            ..Default::default()
        }
    } else {
        DockerCredentials {
            username: Some(user),
            password: Some(secret),
            serveraddress: Some(server),
            ..Default::default()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        let s = |i: &str| split_reference(i);
        assert_eq!(s("alpine"), ("alpine".into(), "latest".into()));
        assert_eq!(s("alpine:3"), ("alpine".into(), "3".into()));
        assert_eq!(
            s("localhost:5000/a/b"),
            ("localhost:5000/a/b".into(), "latest".into())
        );
        assert_eq!(
            s("localhost:5000/a/b:v1"),
            ("localhost:5000/a/b".into(), "v1".into())
        );
        assert_eq!(
            s("ghcr.io/x/y@sha256:abc"),
            ("ghcr.io/x/y".into(), "sha256:abc".into())
        );
    }

    #[test]
    fn registries() {
        assert_eq!(registry_of("alpine"), "docker.io");
        assert_eq!(registry_of("docker.io/library/alpine"), "docker.io");
        assert_eq!(registry_of("bitnami/redis"), "docker.io");
        assert_eq!(registry_of("ghcr.io/a/b"), "ghcr.io");
        assert_eq!(registry_of("localhost:5000/a"), "localhost:5000");
        assert_eq!(key_host("https://index.docker.io/v1/"), "docker.io");
    }

    #[test]
    fn auth_entries() {
        let cfg: Value = serde_json::json!({
            "auths": {
                "https://index.docker.io/v1/": { "auth": "dXNlcjpwYXNz" },
                "ghcr.io": { "identitytoken": "tok" }
            }
        });
        let c = from_config(&cfg, "docker.io").unwrap();
        assert_eq!(c.username.as_deref(), Some("user"));
        assert_eq!(c.password.as_deref(), Some("pass"));
        let c = from_config(&cfg, "ghcr.io").unwrap();
        assert_eq!(c.identitytoken.as_deref(), Some("tok"));
        assert!(from_config(&cfg, "quay.io").is_none());
    }
}
