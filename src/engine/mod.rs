//! Container engines. Docker is the primary engine; kiri is also
//! supported. Both speak the Docker Engine API, so one client (`bollard`)
//! drives them over their Unix sockets: Docker at `/var/run/docker.sock`
//! and kiri at `/run/kiri/docker.sock`. No engine CLI is needed except for image
//! builds. The differences are in how a container joins its pod's
//! namespaces; see [`spec`].

pub mod auth;
pub mod build;
pub mod exec;
pub mod spec;

pub use spec::{create_spec, hash_of};

use crate::errors::{Error, Result};
use crate::project::{HealthOnFailure, RuntimeKind};
use bollard::Docker;
pub use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, ContainerInspectResponse, VolumeCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, KillContainerOptions,
    ListContainersOptions, LogsOptions, RemoveContainerOptions, RemoveVolumeOptions,
    RestartContainerOptions, StartContainerOptions, StatsOptions, StopContainerOptions, TopOptions,
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

pub const LABEL_PROJECT: &str = "io.gantry.project";
pub const LABEL_SERVICE: &str = "io.gantry.service";
pub const LABEL_POD: &str = "io.gantry.pod";
pub const LABEL_POD_ID: &str = "io.gantry.pod-id";
pub const LABEL_HASH: &str = "io.gantry.config-hash";
/// The service's `health_on_failure:`, set only when it is not `none`.
pub const LABEL_HEAL: &str = "io.gantry.health-on-failure";

/// Name the Docker runtime shim is registered under.
pub const DOCKER_RUNTIME: &str = "gantry";

/// Per request timeout. Long because `stop` waits for the grace period.
const REQUEST_TIMEOUT_SECS: u64 = 900;

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
}

/// One reading of a container's resource counters ([`Engine::sample`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// CPU time used since the container started, in nanoseconds.
    pub cpu_ns: Option<u64>,
    /// Memory in use, without reclaimable page cache.
    pub memory: Option<u64>,
    pub memory_limit: Option<u64>,
    pub pids: Option<u64>,
    /// Bytes read from and written to block devices since start.
    pub block_read: u64,
    pub block_write: u64,
    /// CFS scheduler periods, and those in which the CPU limit held the
    /// container back.
    pub periods: u64,
    pub throttled_periods: u64,
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
    /// Consecutive failed healthchecks, when the container has one.
    pub failing_streak: Option<i64>,
    /// The `io.gantry.health-on-failure` label.
    pub heal: HealthOnFailure,
    pub exit_code: Option<i64>,
    pub image: String,
    /// A `gantry run` container rather than the service's own.
    pub oneoff: bool,
    /// Bind and volume mounts as the engine reports them.
    pub mounts: Vec<MountInfo>,
}

/// One mount of a container: where its data lives on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    /// `bind`, `volume`, `tmpfs` and so on.
    pub kind: String,
    /// The volume name, for volume mounts.
    pub name: Option<String>,
    /// Host path of the data (a volume's mountpoint for volume mounts).
    pub source: String,
    /// Path inside the container.
    pub destination: String,
}

/// The default API socket of each runtime, overridable with
/// `GANTRY_DOCKER_HOST` and `GANTRY_KIRI_HOST`.
pub fn default_host(kind: RuntimeKind) -> String {
    let (var, default) = match kind {
        RuntimeKind::Docker => ("GANTRY_DOCKER_HOST", "unix:///var/run/docker.sock"),
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
        let socket = socket_path(&self.host);
        if !socket_alive(&socket).await {
            return match self.kind {
                RuntimeKind::Docker => Err(Error::engine(format!(
                    "docker: cannot reach {}; is dockerd running?",
                    socket.display()
                ))),
                RuntimeKind::Kiri => Err(Error::engine(format!(
                    "kiri: cannot reach {}; is kiri.service running?",
                    socket.display()
                ))),
            };
        }
        let path = socket.display().to_string();
        let fail = |e: bollard::errors::Error| Error::engine(format!("{} {path}: {e}", self.kind));
        let docker =
            Docker::connect_with_unix(&path, REQUEST_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)
                .map_err(fail)?
                .negotiate_version()
                .await
                .map_err(fail)?;
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
                // Pod namespaces reach the shim through HostConfig.Annotations,
                // which older API versions silently drop.
                let v = d.client_version();
                if (v.major_version, v.minor_version) < (1, 43) {
                    return Err(Error::engine(format!(
                        "docker API {v} is too old; Gantry needs 1.43 or later (Docker 24+)"
                    )));
                }
                Ok(())
            }
            _ => d.ping().await.map(drop).map_err(|e| self.err("ping", e)),
        }
    }

    pub async fn image_exists(&self, image: &str) -> Result<bool> {
        Ok(self.image_id(image).await?.is_some())
    }

    /// The local image's ID, or None when it is not present.
    pub async fn image_id(&self, image: &str) -> Result<Option<String>> {
        match self.docker().await?.inspect_image(image).await {
            Ok(i) => Ok(Some(i.id.unwrap_or_default())),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(self.err(&format!("image {image}"), e)),
        }
    }

    /// Pulls an image, with registry credentials from the usual auth
    /// files when there are any.
    pub async fn pull(&self, image: &str, platform: Option<&str>) -> Result<()> {
        let (name, tag) = auth::split_reference(image);
        let creds = auth::credentials_for(self.kind, &name).await;
        let d = self.docker().await?;
        let opts = CreateImageOptions {
            from_image: Some(name.clone()),
            tag: Some(tag.clone()),
            platform: platform.unwrap_or_default().to_string(),
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
        // Some engines report a failed pull only in fields this client does
        // not model, so confirm the image is really there.
        if self.image_id(image).await?.is_none() {
            return Err(Error::engine(format!(
                "{} pull {image}: the engine reported no error but the image is not present{}",
                self.kind,
                if last.is_empty() {
                    String::new()
                } else {
                    format!(" (last status: {last})")
                }
            )));
        }
        Ok(())
    }

    pub async fn create(
        &self,
        name: &str,
        body: ContainerCreateBody,
        platform: Option<&str>,
    ) -> Result<String> {
        let opts = CreateContainerOptions {
            name: Some(name.to_string()),
            platform: platform.unwrap_or_default().to_string(),
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

    /// Restarts a container, waiting up to its own stop timeout before
    /// killing it.
    pub async fn restart(&self, name: &str) -> Result<()> {
        self.docker()
            .await?
            .restart_container(name, None::<RestartContainerOptions>)
            .await
            .map_err(|e| self.err(&format!("restart {name}"), e))
    }

    /// Running Gantry containers that carry a `health_on_failure` action,
    /// inspected. Containers that vanish while listing are skipped.
    pub async fn list_healing(&self) -> Result<Vec<ContainerInfo>> {
        let d = self.docker().await?;
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![LABEL_HEAL.to_string()]);
        filters.insert("status".to_string(), vec!["running".to_string()]);
        let opts = ListContainersOptions {
            all: false,
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

    /// Force-removes a container and its anonymous volumes. Named volumes
    /// and bind mounts are untouched.
    pub async fn remove(&self, name: &str) -> Result<()> {
        let opts = RemoveContainerOptions {
            force: true,
            v: true,
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

    /// One reading of a container's resource counters. Counters are
    /// cumulative, so a rate needs two readings.
    pub async fn sample(&self, name: &str) -> Result<Sample> {
        let opts = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let docker = self.docker().await?;
        let mut stream = docker.stats(name, Some(opts));
        let next = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .map_err(|_| Error::Engine(format!("{}: stats {name}: timed out", self.kind)))?;
        let s = match next {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(self.err(&format!("stats {name}"), e)),
            None => {
                return Err(Error::Engine(format!(
                    "{}: stats {name}: no data",
                    self.kind
                )));
            }
        };
        let cpu = s.cpu_stats.unwrap_or_default();
        let throttling = cpu.throttling_data.unwrap_or_default();
        let mem = s.memory_stats.unwrap_or_default();
        // Page cache that can be dropped is not counted, as `docker stats`
        // does (cgroup v2 names it inactive_file, v1 total_inactive_file).
        let inactive = mem
            .stats
            .as_ref()
            .and_then(|m| {
                m.get("inactive_file")
                    .or_else(|| m.get("total_inactive_file"))
            })
            .copied()
            .unwrap_or(0);
        let (mut read, mut write) = (0, 0);
        for e in s
            .blkio_stats
            .and_then(|b| b.io_service_bytes_recursive)
            .unwrap_or_default()
        {
            match e.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("read") => read += e.value.unwrap_or(0),
                Some("write") => write += e.value.unwrap_or(0),
                _ => {}
            }
        }
        Ok(Sample {
            cpu_ns: cpu.cpu_usage.and_then(|u| u.total_usage),
            memory: mem.usage.map(|u| u.saturating_sub(inactive)),
            memory_limit: mem.limit,
            pids: s.pids_stats.and_then(|p| p.current),
            block_read: read,
            block_write: write,
            periods: throttling.periods.unwrap_or(0),
            throttled_periods: throttling.throttled_periods.unwrap_or(0),
        })
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

    /// The socket this engine talks to.
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
        failing_streak: state
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.failing_streak),
        heal: HealthOnFailure::parse(&label(LABEL_HEAL)).unwrap_or_default(),
        exit_code: state.and_then(|s| s.exit_code),
        image: c
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .unwrap_or_default(),
        oneoff: !label(exec::LABEL_ONEOFF).is_empty(),
        mounts: c
            .mounts
            .iter()
            .flatten()
            .map(|m| MountInfo {
                kind: m.typ.as_ref().map(ToString::to_string).unwrap_or_default(),
                name: m.name.clone().filter(|n| !n.is_empty()),
                source: m.source.clone().unwrap_or_default(),
                destination: m.destination.clone().unwrap_or_default(),
            })
            .collect(),
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
