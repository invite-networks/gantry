//! Image builds. Builds go through the engine's own CLI (`docker build`,
//! `podman build`) rather than the API: the CLI brings BuildKit,
//! `.dockerignore` handling and build secrets, and a build takes seconds
//! to minutes, so the cost of one process does not matter.

use super::{Engine, LABEL_PROJECT, spec::service_image};
use crate::errors::{Error, Result};
use crate::pod::state;
use crate::project::{RuntimeKind, Service};
use std::process::Stdio;

/// The CLI arguments for building `svc`'s image.
pub fn build_args(svc: &Service, project: &str, dockerfile: Option<&str>) -> Vec<String> {
    let mut a = vec!["build".to_string()];
    let Some(b) = &svc.build else {
        return a;
    };
    let mut push = |k: &str, v: String| {
        a.push(k.into());
        a.push(v);
    };
    push("-t", service_image(svc));
    for t in &b.tags {
        push("-t", t.clone());
    }
    // The CLI resolves a relative -f against its working directory;
    // Compose resolves it against the build context.
    let file = dockerfile.map(str::to_string).or_else(|| {
        b.dockerfile.as_ref().map(|f| {
            let p = std::path::Path::new(f);
            if p.is_absolute() {
                f.clone()
            } else {
                b.context.join(p).display().to_string()
            }
        })
    });
    if let Some(f) = file {
        push("-f", f);
    }
    for (k, v) in &b.args {
        push("--build-arg", format!("{k}={v}"));
    }
    if let Some(t) = &b.target {
        push("--target", t.clone());
    }
    push("--label", format!("{LABEL_PROJECT}={project}"));
    for (k, v) in &b.labels {
        push("--label", format!("{k}={v}"));
    }
    if let Some(n) = &b.network {
        push("--network", n.clone());
    }
    for c in &b.cache_from {
        push("--cache-from", c.clone());
    }
    for h in &b.extra_hosts {
        push("--add-host", h.clone());
    }
    if let Some(s) = b.shm_size {
        push("--shm-size", s.to_string());
    }
    if let Some(p) = &svc.platform {
        push("--platform", p.clone());
    }
    if b.pull {
        a.push("--pull".into());
    }
    if b.no_cache {
        a.push("--no-cache".into());
    }
    a.push(b.context.display().to_string());
    a
}

impl Engine {
    /// Builds a service's image, streaming the build output.
    pub async fn build(&self, svc: &Service, project: &str) -> Result<()> {
        let Some(b) = &svc.build else {
            return Ok(());
        };
        if !b.context.is_dir() {
            return Err(Error::config(format!(
                "service {}: build context {} is not a directory",
                svc.name,
                b.context.display()
            )));
        }
        // An inline Dockerfile goes through a file so every engine accepts it.
        let inline = match &b.dockerfile_inline {
            Some(text) => {
                let dir = state::run_dir().join("build");
                state::ensure_dir(&dir)?;
                let path = dir.join(format!("{project}-{}.Dockerfile", svc.name));
                crate::util::write_private(&path, text.as_bytes())?;
                Some(path)
            }
            None => None,
        };
        let inline_str = inline.as_ref().map(|p| p.display().to_string());
        let args = build_args(svc, project, inline_str.as_deref());

        let mut cmd = match self.kind {
            RuntimeKind::Podman => tokio::process::Command::new("podman"),
            RuntimeKind::Docker | RuntimeKind::Kiri => {
                let mut c = tokio::process::Command::new("docker");
                c.arg("-H")
                    .arg(format!("unix://{}", self.socket().await?.display()));
                if self.kind == RuntimeKind::Kiri {
                    // kiri implements the classic build endpoint, not BuildKit
                    // sessions.
                    c.env("DOCKER_BUILDKIT", "0");
                }
                c
            }
        };
        cmd.args(&args).stdin(Stdio::null()).kill_on_drop(true);
        let status = cmd.status().await.map_err(|e| {
            Error::engine(format!(
                "{}: building {} needs the {} CLI: {e}",
                svc.name,
                svc.image,
                if self.kind == RuntimeKind::Podman {
                    "podman"
                } else {
                    "docker"
                }
            ))
        })?;
        if let Some(p) = inline {
            let _ = std::fs::remove_file(p);
        }
        if status.success() {
            Ok(())
        } else {
            Err(Error::engine(format!(
                "{}: build of {} failed ({status})",
                svc.name, svc.image
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{LoadOptions, load};

    #[test]
    fn dockerfile_resolves_against_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("compose.yaml"),
            "name: b\nservices:\n  a:\n    build: { context: ./app, dockerfile: Dockerfile.prod, args: { V: \"1\" } }\n    network_mode: none\n",
        )
        .unwrap();
        let p = load(&LoadOptions {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap()
        .project;
        let a = build_args(&p.services["a"], "b", None);
        let ctx = dir.path().join("app");
        let f = a.iter().position(|x| x == "-f").unwrap();
        assert_eq!(a[f + 1], ctx.join("Dockerfile.prod").display().to_string());
        assert!(a.contains(&"V=1".to_string()));
        assert_eq!(a.last().unwrap(), &ctx.display().to_string());
        // An inline Dockerfile path wins.
        let a = build_args(&p.services["a"], "b", Some("/run/x.Dockerfile"));
        let f = a.iter().position(|x| x == "-f").unwrap();
        assert_eq!(a[f + 1], "/run/x.Dockerfile");
    }
}
