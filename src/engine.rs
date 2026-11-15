//! Container engines. Docker, Podman and kiri all take the same CLI
//! shape, so one implementation drives all three through their CLIs:
//! `docker`, `podman`, and `docker -H unix:///run/kiri/docker.sock`.
//! The differences are in how a container joins its pod's namespaces.

use crate::compose::units::cli_duration;
use crate::errors::{Error, Result};
use crate::oci;
use crate::pod::state::PodState;
use crate::project::{MountKind, Project, RuntimeKind, Service};
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

pub const LABEL_PROJECT: &str = "io.kirisame.project";
pub const LABEL_SERVICE: &str = "io.kirisame.service";
pub const LABEL_POD: &str = "io.kirisame.pod";
pub const LABEL_POD_ID: &str = "io.kirisame.pod-id";
pub const LABEL_HASH: &str = "io.kirisame.config-hash";

/// Name the Docker runtime shim is registered under.
pub const DOCKER_RUNTIME: &str = "kirisame";

#[derive(Debug, Clone)]
pub struct Engine {
    pub kind: RuntimeKind,
    bin: String,
    base: Vec<String>,
}

/// A container as reported by `inspect`.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub service: String,
    pub pod: String,
    pub pod_id: String,
    pub hash: String,
    pub status: String,
    pub health: Option<String>,
    pub exit_code: Option<i64>,
    pub image: String,
}

impl Engine {
    pub fn new(kind: RuntimeKind) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        match kind {
            RuntimeKind::Docker => Self {
                kind,
                bin: env("KIRISAME_DOCKER").unwrap_or_else(|| "docker".into()),
                base: vec![],
            },
            RuntimeKind::Podman => Self {
                kind,
                bin: env("KIRISAME_PODMAN").unwrap_or_else(|| "podman".into()),
                base: vec![],
            },
            RuntimeKind::Kiri => Self {
                kind,
                bin: env("KIRISAME_DOCKER").unwrap_or_else(|| "docker".into()),
                base: vec![
                    "-H".into(),
                    env("KIRISAME_KIRI_HOST")
                        .unwrap_or_else(|| "unix:///run/kiri/docker.sock".into()),
                ],
            },
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(&self.base);
        c.kill_on_drop(true);
        c
    }

    /// A std::process::Command for interactive use (exec, logs).
    pub fn std_cmd(&self) -> std::process::Command {
        let mut c = std::process::Command::new(&self.bin);
        c.args(&self.base);
        c
    }

    /// Runs the CLI and returns stdout, or an error carrying stderr.
    pub async fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let args: Vec<std::ffi::OsString> = args
            .into_iter()
            .map(|a| a.as_ref().to_os_string())
            .collect();
        let out = self
            .cmd()
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| Error::engine(format!("{} ({}): {e}", self.bin, self.kind)))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            let what = args
                .first()
                .map(|a| a.to_string_lossy().to_string())
                .unwrap_or_default();
            Err(Error::engine(format!(
                "{} {what}: {}",
                self.kind,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    /// Checks the engine is reachable and, for Docker, that the kirisame
    /// runtime shim is registered.
    pub async fn check(&self) -> Result<()> {
        match self.kind {
            RuntimeKind::Docker => {
                let out = self.run(["info", "--format", "{{json .Runtimes}}"]).await?;
                let runtimes: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
                if runtimes.get(DOCKER_RUNTIME).is_none() {
                    return Err(Error::engine(format!(
                        "docker has no {DOCKER_RUNTIME:?} runtime; run `kirisame setup docker` once on this host"
                    )));
                }
                Ok(())
            }
            _ => self.run(["version"]).await.map(drop),
        }
    }

    pub async fn image_exists(&self, image: &str) -> bool {
        self.run(["image", "inspect", image]).await.is_ok()
    }

    /// Pulls an image, showing the engine's progress output.
    pub async fn pull(&self, image: &str) -> Result<()> {
        let status = self
            .cmd()
            .args(["pull", image])
            .stdin(Stdio::null())
            .status()
            .await
            .map_err(|e| Error::engine(format!("{}: {e}", self.bin)))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::engine(format!("{} pull {image} failed", self.kind)))
        }
    }

    /// The image's default command, used when a multi word entrypoint has
    /// to be split for the docker CLI.
    pub async fn image_cmd(&self, image: &str) -> Result<Vec<String>> {
        let out = self
            .run([
                "image",
                "inspect",
                "--format",
                "{{json .Config.Cmd}}",
                image,
            ])
            .await?;
        Ok(serde_json::from_str::<Option<Vec<String>>>(out.trim())
            .ok()
            .flatten()
            .unwrap_or_default())
    }

    pub async fn create(&self, args: &[String]) -> Result<String> {
        let mut full = vec!["create".to_string()];
        full.extend_from_slice(args);
        Ok(self.run(&full).await?.trim().to_string())
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        self.run(["start", name]).await.map(drop)
    }

    pub async fn stop(&self, name: &str, timeout: Option<u64>) -> Result<()> {
        let mut args = vec!["stop".to_string()];
        if let Some(t) = timeout {
            args.push("-t".into());
            args.push(t.to_string());
        }
        args.push(name.into());
        self.run(&args).await.map(drop)
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        self.run(["rm", "-f", name]).await.map(drop)
    }

    /// Containers labelled with the project (all states).
    pub async fn list(&self, project: &str) -> Result<Vec<ContainerInfo>> {
        let ids = self
            .run([
                "ps",
                "-a",
                "-q",
                "--no-trunc",
                "--filter",
                &format!("label={LABEL_PROJECT}={project}"),
            ])
            .await?;
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut args = vec!["inspect"];
        args.extend(ids);
        let out = self.run(&args).await?;
        let list: Vec<Value> =
            serde_json::from_str(&out).map_err(|e| Error::engine(format!("parse inspect: {e}")))?;
        Ok(list.iter().map(container_info).collect())
    }

    pub async fn inspect(&self, name: &str) -> Result<Option<ContainerInfo>> {
        match self.run(["inspect", "--type", "container", name]).await {
            Ok(out) => {
                let list: Vec<Value> = serde_json::from_str(&out)
                    .map_err(|e| Error::engine(format!("parse inspect: {e}")))?;
                Ok(list.first().map(container_info))
            }
            Err(Error::Engine(e)) if e.contains("No such") || e.contains("no such") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn volume_ensure(
        &self,
        name: &str,
        driver: Option<&str>,
        opts: &[(String, String)],
        project: &str,
    ) -> Result<()> {
        if self.run(["volume", "inspect", name]).await.is_ok() {
            return Ok(());
        }
        let mut args = vec![
            "volume".to_string(),
            "create".into(),
            "--label".into(),
            format!("{LABEL_PROJECT}={project}"),
        ];
        if let Some(d) = driver {
            args.push("--driver".into());
            args.push(d.into());
        }
        for (k, v) in opts {
            args.push("--opt".into());
            args.push(format!("{k}={v}"));
        }
        args.push(name.into());
        self.run(&args).await.map(drop)
    }

    pub async fn volume_remove(&self, name: &str) -> Result<()> {
        self.run(["volume", "rm", name]).await.map(drop)
    }
}

fn container_info(v: &Value) -> ContainerInfo {
    let s = |p: &str| {
        v.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let label = |k: &str| {
        v.pointer("/Config/Labels")
            .and_then(|l| l.get(k))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let health = v
        .pointer("/State/Health/Status")
        .or_else(|| v.pointer("/State/Healthcheck/Status"))
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    ContainerInfo {
        id: s("/Id"),
        name: s("/Name").trim_start_matches('/').to_string(),
        service: label(LABEL_SERVICE),
        pod: label(LABEL_POD),
        pod_id: label(LABEL_POD_ID),
        hash: label(LABEL_HASH),
        status: s("/State/Status"),
        health,
        exit_code: v.pointer("/State/ExitCode").and_then(Value::as_i64),
        image: s("/Config/Image"),
    }
}

/// Builds the `create` arguments for a service in its pod. The config
/// hash label is computed over everything else and appended last.
pub fn create_args(
    project: &Project,
    svc: &Service,
    pod: &PodState,
    image_cmd: Option<Vec<String>>,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |k: &str, v: String| {
        a.push(k.to_string());
        a.push(v);
    };
    push("--name", svc.container_name.clone());
    push("--label", format!("{LABEL_PROJECT}={}", project.name));
    push("--label", format!("{LABEL_SERVICE}={}", svc.name));
    push("--label", format!("{LABEL_POD}={}", svc.pod));
    push("--label", format!("{LABEL_POD_ID}={}", pod.id));
    for (k, v) in &svc.labels {
        push("--label", format!("{k}={v}"));
    }

    // Joining the pod.
    let netns = pod.netns.display().to_string();
    match svc.runtime {
        RuntimeKind::Docker => {
            push("--runtime", DOCKER_RUNTIME.into());
            push("--network", "none".into());
            push("--annotation", format!("{}={netns}", oci::ANN_NETNS));
            if let Some(u) = &pod.uts {
                push(
                    "--annotation",
                    format!("{}={}", oci::ANN_UTSNS, u.display()),
                );
            } else {
                push("--hostname", pod.hostname.clone());
            }
            if let Some(i) = &pod.ipc {
                push(
                    "--annotation",
                    format!("{}={}", oci::ANN_IPCNS, i.display()),
                );
            }
        }
        RuntimeKind::Podman => {
            push("--network", format!("ns:{netns}"));
            match &pod.uts {
                Some(u) => push("--uts", format!("ns:{}", u.display())),
                None => push("--hostname", pod.hostname.clone()),
            }
            if let Some(i) = &pod.ipc {
                push("--ipc", format!("ns:{}", i.display()));
            }
        }
        RuntimeKind::Kiri => {
            // kiri joins the network namespace; UTS and IPC stay per container.
            push("--network", format!("ns:{netns}"));
            push("--hostname", pod.hostname.clone());
        }
    }
    for (src, dst) in crate::pod::etc_mounts(pod) {
        if svc.runtime == RuntimeKind::Kiri && dst == "/etc/hostname" {
            continue;
        }
        push("-v", format!("{}:{dst}:ro", src.display()));
    }

    for (k, v) in &svc.environment {
        push("-e", format!("{k}={v}"));
    }
    if let Some(w) = &svc.working_dir {
        push("-w", w.clone());
    }
    if let Some(u) = &svc.user {
        push("-u", u.clone());
    }
    for m in &svc.mounts {
        match m.kind {
            MountKind::Bind | MountKind::Volume => {
                let mut opts: Vec<String> = Vec::new();
                if m.read_only {
                    opts.push("ro".into());
                }
                opts.extend(m.options.iter().cloned());
                let mut spec = if m.source.is_empty() {
                    m.target.clone()
                } else {
                    format!("{}:{}", m.source, m.target)
                };
                if !opts.is_empty() && !m.source.is_empty() {
                    spec.push(':');
                    spec.push_str(&opts.join(","));
                }
                push("-v", spec);
            }
            MountKind::Tmpfs => {
                let spec = if m.options.is_empty() {
                    m.target.clone()
                } else {
                    format!("{}:{}", m.target, m.options.join(","))
                };
                push("--tmpfs", spec);
            }
        }
    }
    if let Some(r) = &svc.restart {
        push("--restart", r.clone());
    }
    if let Some(h) = &svc.healthcheck {
        if h.disabled {
            a.push("--no-healthcheck".into());
        } else {
            let mut push = |k: &str, v: String| {
                a.push(k.to_string());
                a.push(v);
            };
            if let Some(t) = &h.test {
                let cmd = match t.first().map(String::as_str) {
                    Some("CMD-SHELL") => t[1..].join(" "),
                    _ => shell_words::join(&t[1..]),
                };
                push("--health-cmd", cmd);
            }
            if let Some(d) = h.interval {
                push("--health-interval", cli_duration(d));
            }
            if let Some(d) = h.timeout {
                push("--health-timeout", cli_duration(d));
            }
            if let Some(r) = h.retries {
                push("--health-retries", r.to_string());
            }
            if let Some(d) = h.start_period {
                push("--health-start-period", cli_duration(d));
            }
            if let Some(d) = h.start_interval {
                if svc.runtime != RuntimeKind::Podman {
                    push("--health-start-interval", cli_duration(d));
                }
            }
        }
    }
    let mut push = |k: &str, v: String| {
        a.push(k.to_string());
        a.push(v);
    };
    if let Some(s) = &svc.stop_signal {
        push("--stop-signal", s.clone());
    }
    if let Some(d) = svc.stop_grace_period {
        push("--stop-timeout", d.as_secs().max(1).to_string());
    }
    for c in &svc.cap_add {
        push("--cap-add", c.clone());
    }
    for c in &svc.cap_drop {
        push("--cap-drop", c.clone());
    }
    for d in &svc.devices {
        push("--device", d.clone());
    }
    if let Some(m) = svc.mem_limit {
        push("--memory", m.to_string());
    }
    if let Some(c) = svc.cpus {
        push("--cpus", c.to_string());
    }
    if let Some(p) = svc.pids_limit {
        push("--pids-limit", p.to_string());
    }
    if let Some(s) = svc.shm_size {
        push("--shm-size", s.to_string());
    }
    for (k, (soft, hard)) in &svc.ulimits {
        push("--ulimit", format!("{k}={soft}:{hard}"));
    }
    for s in &svc.security_opt {
        push("--security-opt", s.clone());
    }
    for g in &svc.group_add {
        push("--group-add", g.clone());
    }
    for (k, v) in &svc.sysctls {
        push("--sysctl", format!("{k}={v}"));
    }
    for (flag, on) in [
        ("--privileged", svc.privileged),
        ("--read-only", svc.read_only),
        ("-t", svc.tty),
        ("-i", svc.stdin_open),
        ("--init", svc.init),
    ] {
        if on {
            a.push(flag.into());
        }
    }
    a.push("--pull".into());
    a.push("never".into());

    // Entrypoint and command. The docker CLI only takes a single word
    // entrypoint; podman takes a JSON array.
    let mut command = svc.command.clone();
    if let Some(ep) = &svc.entrypoint {
        match (svc.runtime, ep.len()) {
            (_, 0) => {
                a.push("--entrypoint".into());
                a.push(String::new());
            }
            (RuntimeKind::Podman, _) => {
                a.push("--entrypoint".into());
                a.push(serde_json::to_string(ep).unwrap_or_default());
            }
            (_, 1) => {
                a.push("--entrypoint".into());
                a.push(ep[0].clone());
            }
            _ => {
                a.push("--entrypoint".into());
                a.push(ep[0].clone());
                let mut rest = ep[1..].to_vec();
                rest.extend(command.take().or(image_cmd).unwrap_or_default());
                command = Some(rest);
            }
        }
    }
    let hash = crate::util::hash_hex(
        serde_json::to_string(&(&a, &svc.image, &command))
            .unwrap_or_default()
            .as_bytes(),
    );
    a.push("--label".into());
    a.push(format!("{LABEL_HASH}={hash}"));
    a.push(svc.image.clone());
    a.extend(command.unwrap_or_default());
    a
}

/// Reads the config hash back out of create args.
pub fn hash_of(args: &[String]) -> String {
    args.iter()
        .find_map(|a| a.strip_prefix(&format!("{LABEL_HASH}=")))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{LoadOptions, load};
    use std::path::PathBuf;

    fn project(runtime: &str) -> Project {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("compose.yaml"),
            format!(
                r#"
name: demo
runtime: {runtime}
networks:
  lan: {{ driver: macvlan, parent: eth9, ipam: {{ config: [{{ subnet: 192.0.2.0/24, gateway: 192.0.2.1 }}] }} }}
pods:
  web:
    networks: {{ lan: {{ ipv4_address: 192.0.2.10 }} }}
services:
  app:
    image: alpine:3
    pod: web
    entrypoint: [/bin/sh, -c]
    command: ["echo hi"]
    environment: {{ A: "1" }}
    volumes: ["./data:/data:ro", "/srv/x:/x"]
    tmpfs: ["/tmp:size=64m"]
    healthcheck: {{ test: ["CMD", "true"], interval: 5s }}
    restart: unless-stopped
"#
            ),
        )
        .unwrap();
        load(&LoadOptions {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap()
        .project
    }

    fn state(uts: bool) -> PodState {
        PodState {
            id: "abc".into(),
            project: "demo".into(),
            name: "web".into(),
            config_hash: "h".into(),
            netns: PathBuf::from("/run/netns/ks-demo-web"),
            uts: uts.then(|| PathBuf::from("/run/kirisame/demo/pods/web/uts")),
            ipc: Some(PathBuf::from("/run/kirisame/demo/pods/web/ipc")),
            hostname: "web".into(),
            interfaces: vec![],
            created: 0,
        }
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn docker_args() {
        let p = project("docker");
        let a = create_args(&p, &p.services["app"], &state(true), None);
        let s = joined(&a);
        assert!(s.contains("--runtime kirisame --network none --annotation io.kirisame.netns=/run/netns/ks-demo-web"), "{s}");
        assert!(s.contains("--annotation io.kirisame.utsns=/run/kirisame/demo/pods/web/uts"));
        assert!(!s.contains("--hostname"));
        assert!(s.contains("/etc/hosts:ro"));
        assert!(s.contains("--entrypoint /bin/sh"));
        assert!(s.ends_with("alpine:3 -c echo hi"), "{s}");
        assert!(s.contains("--health-cmd true --health-interval 5s"));
        assert!(s.contains("--tmpfs /tmp:size=64m"));
        assert!(s.contains(":/data:ro"));
        assert!(!hash_of(&a).is_empty());
    }

    #[test]
    fn podman_and_kiri_args() {
        let p = project("podman");
        let s = joined(&create_args(&p, &p.services["app"], &state(true), None));
        assert!(s.contains("--network ns:/run/netns/ks-demo-web --uts ns:/run/kirisame/demo/pods/web/uts --ipc ns:"), "{s}");
        assert!(s.contains(r#"--entrypoint ["/bin/sh","-c"]"#), "{s}");
        assert!(s.ends_with("alpine:3 echo hi"), "{s}");

        let p = project("kiri");
        let s = joined(&create_args(&p, &p.services["app"], &state(true), None));
        assert!(
            s.contains("--network ns:/run/netns/ks-demo-web --hostname web"),
            "{s}"
        );
        assert!(!s.contains("/etc/hostname"));
    }

    #[test]
    fn hash_changes_with_pod_id() {
        let p = project("docker");
        let a = create_args(&p, &p.services["app"], &state(true), None);
        let mut st = state(true);
        st.id = "other".into();
        let b = create_args(&p, &p.services["app"], &st, None);
        assert_ne!(hash_of(&a), hash_of(&b));
    }
}
