//! Container engines. Docker, Podman and kiri all speak the Docker Engine
//! API, so one client (`bollard`) drives all three over their Unix
//! sockets: Docker at `/var/run/docker.sock`, kiri at
//! `/run/kiri/docker.sock`, and Podman through its Docker-compatible API
//! at `/run/podman/podman.sock`. No engine CLI is needed except for image
//! builds. The differences are in how a container joins its pod's
//! namespaces; see [`spec`].

pub mod auth;
pub mod build;
pub mod exec;
pub mod spec;

pub use spec::{create_spec, hash_of, qualify_image};

use crate::errors::{Error, Result};
use crate::pod::state;
use crate::project::RuntimeKind;
use bollard::Docker;
pub use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, ContainerInspectResponse, VolumeCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, KillContainerOptions,
    ListContainersOptions, LogsOptions, RemoveContainerOptions, RemoveVolumeOptions,
    StartContainerOptions, StopContainerOptions, TopOptions,
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;

pub const LABEL_PROJECT: &str = "io.gantry.project";
pub const LABEL_SERVICE: &str = "io.gantry.service";
pub const LABEL_POD: &str = "io.gantry.pod";
pub const LABEL_POD_ID: &str = "io.gantry.pod-id";
pub const LABEL_HASH: &str = "io.gantry.config-hash";

/// Name the Docker runtime shim is registered under.
pub const DOCKER_RUNTIME: &str = "gantry";

/// Per request timeout. Long because `stop` waits for the grace period.
const REQUEST_TIMEOUT_SECS: u64 = 900;

/// The Docker API version Podman's compat layer implements.
const PODMAN_COMPAT_API: bollard::ClientVersion = bollard::ClientVersion {
    major_version: 1,
    minor_version: 41,
};

/// How long a private Podman API service stays up when idle.
const PODMAN_SERVICE_IDLE_SECS: u32 = 300;

pub type LogStream = Pin<Box<dyn Stream<Item = Result<LogOutput>> + Send>>;

#[derive(Debug)]
struct Client {
    docker: Docker,
    socket: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Engine {
    pub kind: RuntimeKind,
    host: String,
    client: Arc<OnceCell<Client>>,
    /// Start Podman's API service when its socket is down. Off for engines
    /// a project does not use, so looking never starts anything.
    autostart: bool,
}

/// A container as reported by `inspect`.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    /// The `io.gantry.project` label; empty for containers Gantry did not
    /// create.
    pub project: String,
    pub service: String,
    pub pod: String,
    pub pod_id: String,
    pub hash: String,
    pub status: String,
    pub health: Option<String>,
    pub exit_code: Option<i64>,
    pub image: String,
    /// A `gantry run` container rather than the service's own.
    pub oneoff: bool,
}

/// The default API socket of each runtime, overridable with
/// `GANTRY_DOCKER_HOST`, `GANTRY_PODMAN_HOST` and `GANTRY_KIRI_HOST`.
pub fn default_host(kind: RuntimeKind) -> String {
    let (var, default) = match kind {
        RuntimeKind::Docker => ("GANTRY_DOCKER_HOST", "unix:///var/run/docker.sock"),
        RuntimeKind::Podman => ("GANTRY_PODMAN_HOST", "unix:///run/podman/podman.sock"),
        RuntimeKind::Kiri => ("GANTRY_KIRI_HOST", "unix:///run/kiri/docker.sock"),
    };
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}

fn socket_path(host: &str) -> PathBuf {
    PathBuf::from(host.strip_prefix("unix://").unwrap_or(host))
}

/// True when the API returned this HTTP status.
fn is_status(e: &bollard::errors::Error, code: u16) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code, .. } if *status_code == code)
}

fn not_found(e: &bollard::errors::Error) -> bool {
    is_status(e, 404)
}

impl Engine {
    pub fn new(kind: RuntimeKind) -> Self {
        Self {
            kind,
            host: default_host(kind),
            client: Arc::new(OnceCell::new()),
            autostart: true,
        }
    }

    /// An engine that only connects to a socket that is already up.
    pub fn passive(kind: RuntimeKind) -> Self {
        Self {
            autostart: false,
            ..Self::new(kind)
        }
    }

    fn err(&self, what: &str, e: bollard::errors::Error) -> Error {
        let msg = match e {
            bollard::errors::Error::DockerResponseServerError { message, .. } => message,
            other => other.to_string(),
        };
        Error::engine(format!("{} {what}: {}", self.kind, msg.trim()))
    }

    async fn client(&self) -> Result<&Client> {
        self.client.get_or_try_init(|| self.connect()).await
    }

    async fn docker(&self) -> Result<&Docker> {
        Ok(&self.client().await?.docker)
    }

    async fn connect(&self) -> Result<Client> {
        let t = std::time::Instant::now();
        let r = self.connect_inner().await;
        tracing::debug!("{} connect: {:?}", self.kind, t.elapsed());
        r
    }

    async fn connect_inner(&self) -> Result<Client> {
        let mut socket = socket_path(&self.host);
        if !socket_alive(&socket).await {
            socket = match self.kind {
                RuntimeKind::Podman if self.autostart => start_podman_service(&socket).await?,
                RuntimeKind::Podman => {
                    return Err(Error::engine(format!(
                        "podman: no API service at {}",
                        socket.display()
                    )));
                }
                RuntimeKind::Docker => {
                    return Err(Error::engine(format!(
                        "docker: cannot reach {}; is dockerd running?",
                        socket.display()
                    )));
                }
                RuntimeKind::Kiri => {
                    return Err(Error::engine(format!(
                        "kiri: cannot reach {}; is kiri.service running?",
                        socket.display()
                    )));
                }
            };
        }
        let path = socket.display().to_string();
        let fail = |e: bollard::errors::Error| Error::engine(format!("{} {path}: {e}", self.kind));
        let docker = match self.kind {
            // Podman's /version gathers host information and takes about
            // 160 ms; its compat API has implemented version 1.41 for years,
            // so use that directly.
            RuntimeKind::Podman => {
                Docker::connect_with_unix(&path, REQUEST_TIMEOUT_SECS, &PODMAN_COMPAT_API)
                    .map_err(fail)?
            }
            _ => {
                Docker::connect_with_unix(&path, REQUEST_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                    .map_err(fail)?
                    .negotiate_version()
                    .await
                    .map_err(fail)?
            }
        };
        Ok(Client { docker, socket })
    }

    /// Checks the engine is reachable and, for Docker, that the gantry
    /// runtime shim is registered.
    pub async fn check(&self) -> Result<()> {
        let d = self.docker().await?;
        match self.kind {
            RuntimeKind::Docker => {
                let info = d.info().await.map_err(|e| self.err("info", e))?;
                if !info
                    .runtimes
                    .as_ref()
                    .is_some_and(|r| r.contains_key(DOCKER_RUNTIME))
                {
                    return Err(Error::engine(format!(
                        "docker has no {DOCKER_RUNTIME:?} runtime; run `gantry setup docker` once on this host"
                    )));
                }
                Ok(())
            }
            _ => d.ping().await.map(drop).map_err(|e| self.err("ping", e)),
        }
    }

    pub async fn image_exists(&self, image: &str) -> Result<bool> {
        let image = qualify_image(self.kind, image);
        match self.docker().await?.inspect_image(&image).await {
            Ok(_) => Ok(true),
            Err(e) if not_found(&e) => Ok(false),
            Err(e) => Err(self.err(&format!("image {image}"), e)),
        }
    }

    /// Pulls an image, with registry credentials from the usual auth
    /// files when there are any.
    pub async fn pull(&self, image: &str) -> Result<()> {
        let image = qualify_image(self.kind, image);
        let (name, tag) = auth::split_reference(&image);
        let creds = auth::credentials_for(self.kind, &name);
        let d = self.docker().await?;
        let opts = CreateImageOptions {
            from_image: Some(name.clone()),
            tag: Some(tag.clone()),
            ..Default::default()
        };
        let mut stream = d.create_image(Some(opts), None, creds);
        let mut last = String::new();
        while let Some(item) = stream.next().await {
            let info = item.map_err(|e| self.err(&format!("pull {image}"), e))?;
            if let Some(detail) = info.error_detail.and_then(|d| d.message) {
                return Err(Error::engine(format!(
                    "{} pull {image}: {detail}",
                    self.kind
                )));
            }
            if let Some(s) = info.status {
                if info.id.is_none() {
                    last = s;
                }
            }
        }
        if !last.is_empty() {
            tracing::debug!("{image}: {last}");
        }
        Ok(())
    }

    pub async fn create(&self, name: &str, body: ContainerCreateBody) -> Result<String> {
        let opts = CreateContainerOptions {
            name: Some(name.to_string()),
            ..Default::default()
        };
        let r = self
            .docker()
            .await?
            .create_container(Some(opts), body)
            .await
            .map_err(|e| self.err(&format!("create {name}"), e))?;
        for w in r.warnings {
            if !w.is_empty() {
                tracing::warn!("{name}: {w}");
            }
        }
        Ok(r.id)
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        match self
            .docker()
            .await?
            .start_container(name, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_status(&e, 304) => Ok(()),
            Err(e) => Err(self.err(&format!("start {name}"), e)),
        }
    }

    pub async fn stop(&self, name: &str, timeout: Option<u64>) -> Result<()> {
        let opts = StopContainerOptions {
            t: timeout.map(|t| i32::try_from(t).unwrap_or(i32::MAX)),
            signal: None,
        };
        match self.docker().await?.stop_container(name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if is_status(&e, 304) || not_found(&e) => Ok(()),
            Err(e) => Err(self.err(&format!("stop {name}"), e)),
        }
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        let opts = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };
        match self
            .docker()
            .await?
            .remove_container(name, Some(opts))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(self.err(&format!("rm {name}"), e)),
        }
    }

    /// Containers labelled with the project (all states), inspected in
    /// parallel.
    pub async fn list(&self, project: &str) -> Result<Vec<ContainerInfo>> {
        let d = self.docker().await?;
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_PROJECT}={project}")],
        );
        let opts = ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        };
        let list = d
            .list_containers(Some(opts))
            .await
            .map_err(|e| self.err("ps", e))?;
        let ids: Vec<String> = list.into_iter().filter_map(|c| c.id).collect();
        let found = futures_util::future::join_all(ids.iter().map(|id| self.inspect(id))).await;
        let mut out = Vec::new();
        for r in found {
            if let Some(c) = r? {
                out.push(c);
            }
        }
        Ok(out)
    }

    pub async fn inspect(&self, name: &str) -> Result<Option<ContainerInfo>> {
        match self
            .docker()
            .await?
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
        {
            Ok(c) => Ok(Some(container_info(&c))),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(self.err(&format!("inspect {name}"), e)),
        }
    }

    pub async fn volume_ensure(
        &self,
        name: &str,
        driver: Option<&str>,
        opts: &[(String, String)],
        project: &str,
    ) -> Result<()> {
        let d = self.docker().await?;
        match d.inspect_volume(name).await {
            Ok(_) => return Ok(()),
            Err(e) if not_found(&e) => {}
            Err(e) => return Err(self.err(&format!("volume {name}"), e)),
        }
        let req = VolumeCreateRequest {
            name: Some(name.to_string()),
            driver: driver.map(str::to_string),
            driver_opts: Some(opts.iter().cloned().collect()),
            labels: Some(HashMap::from([(
                LABEL_PROJECT.to_string(),
                project.to_string(),
            )])),
            ..Default::default()
        };
        d.create_volume(req)
            .await
            .map(drop)
            .map_err(|e| self.err(&format!("volume create {name}"), e))
    }

    pub async fn volume_remove(&self, name: &str) -> Result<()> {
        self.docker()
            .await?
            .remove_volume(name, None::<RemoveVolumeOptions>)
            .await
            .map_err(|e| self.err(&format!("volume rm {name}"), e))
    }

    /// Container output as a stream of chunks.
    pub async fn logs(
        &self,
        name: &str,
        follow: bool,
        tail: Option<&str>,
        timestamps: bool,
    ) -> Result<LogStream> {
        let opts = LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            timestamps,
            tail: tail.unwrap_or("all").to_string(),
            ..Default::default()
        };
        let kind = self.kind;
        let s = self
            .docker()
            .await?
            .logs(name, Some(opts))
            .map_err(move |e| Error::engine(format!("{kind} logs: {e}")));
        Ok(Box::pin(s))
    }

    /// Runs the container's healthcheck once. Podman runs healthchecks
    /// from systemd timers, which are not always available; this lets a
    /// `service_healthy` wait make progress anyway. A no-op elsewhere.
    pub async fn run_healthcheck(&self, name: &str) -> Result<()> {
        if self.kind != RuntimeKind::Podman {
            return Ok(());
        }
        let socket = self.client().await?.socket.clone();
        let path = format!("/v4.0.0/libpod/containers/{name}/healthcheck");
        raw_get(&socket, &path).await.map(drop)
    }

    pub async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        let opts = KillContainerOptions {
            signal: signal.to_string(),
        };
        match self.docker().await?.kill_container(name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) || is_status(&e, 409) => Ok(()),
            Err(e) => Err(self.err(&format!("kill {name}"), e)),
        }
    }

    /// The processes running in a container: column titles and rows.
    pub async fn top(&self, name: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let r = self
            .docker()
            .await?
            .top_processes(name, None::<TopOptions>)
            .await
            .map_err(|e| self.err(&format!("top {name}"), e))?;
        Ok((
            r.titles.unwrap_or_default(),
            r.processes.unwrap_or_default(),
        ))
    }

    /// The engine's version and the API version in use.
    pub async fn version(&self) -> Result<String> {
        let d = self.docker().await?;
        let v = d.version().await.map_err(|e| self.err("version", e))?;
        Ok(format!(
            "{} (API {})",
            v.version.unwrap_or_default(),
            d.client_version()
        ))
    }

    /// The underlying API client, for exec and builds.
    pub(crate) async fn api(&self) -> Result<&Docker> {
        self.docker().await
    }

    /// The socket this engine talks to (after any Podman fallback).
    pub async fn socket(&self) -> Result<PathBuf> {
        Ok(self.client().await?.socket.clone())
    }
}

fn container_info(c: &ContainerInspectResponse) -> ContainerInfo {
    let labels = c.config.as_ref().and_then(|c| c.labels.as_ref());
    let label = |k: &str| labels.and_then(|l| l.get(k)).cloned().unwrap_or_default();
    let state = c.state.as_ref();
    let health = state
        .and_then(|s| s.health.as_ref())
        .and_then(|h| h.status)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty() && s != "none");
    ContainerInfo {
        id: c.id.clone().unwrap_or_default(),
        name: c
            .name
            .as_deref()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string(),
        project: label(LABEL_PROJECT),
        service: label(LABEL_SERVICE),
        pod: label(LABEL_POD),
        pod_id: label(LABEL_POD_ID),
        hash: label(LABEL_HASH),
        status: state
            .and_then(|s| s.status)
            .map(|s| s.to_string())
            .unwrap_or_default(),
        health,
        exit_code: state.and_then(|s| s.exit_code),
        image: c
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .unwrap_or_default(),
        oneoff: !label(exec::LABEL_ONEOFF).is_empty(),
    }
}

async fn socket_alive(path: &std::path::Path) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::UnixStream::connect(path),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

/// Makes a Podman API socket available: the systemd socket unit first,
/// then a private `podman system service` that exits when idle.
async fn start_podman_service(default: &std::path::Path) -> Result<PathBuf> {
    let started = tokio::process::Command::new("systemctl")
        .args(["start", "podman.socket"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if started && wait_socket(default, Duration::from_secs(3)).await {
        tracing::info!("started podman.socket");
        return Ok(default.to_path_buf());
    }
    let private = state::run_dir().join("podman.sock");
    if socket_alive(&private).await {
        return Ok(private);
    }
    state::ensure_dir(&state::run_dir())?;
    let _ = std::fs::remove_file(&private);
    tracing::info!(
        "starting a private podman API service at {}",
        private.display()
    );
    let mut cmd = std::process::Command::new("podman");
    cmd.args([
        "system",
        "service",
        &format!("--time={PODMAN_SERVICE_IDLE_SECS}"),
        &format!("unix://{}", private.display()),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .process_group(0);
    cmd.spawn().map_err(|e| {
        Error::engine(format!(
            "podman: no API socket at {} and could not start `podman system service`: {e}",
            default.display()
        ))
    })?;
    if wait_socket(&private, Duration::from_secs(10)).await {
        Ok(private)
    } else {
        Err(Error::engine(format!(
            "podman: the API service did not come up at {}",
            private.display()
        )))
    }
}

use std::os::unix::process::CommandExt;

async fn wait_socket(path: &std::path::Path, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if socket_alive(path).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A plain HTTP GET over a Unix socket, for the few libpod endpoints the
/// Docker API has no equivalent for.
async fn raw_get(socket: &std::path::Path, path: &str) -> Result<String> {
    let mut s = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| Error::engine(format!("{}: {e}", socket.display())))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| Error::engine(format!("{}: {e}", socket.display())))?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(120), s.read_to_end(&mut buf))
        .await
        .map_err(|_| Error::engine(format!("{path}: timed out")))?
        .map_err(|e| Error::engine(format!("{path}: {e}")))?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(Error::engine(format!(
            "{path}: HTTP {status}: {}",
            body.trim()
        )))
    }
}
