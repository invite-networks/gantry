//! The commands: bringing pods and their services up and down in order.

use crate::engine::{ContainerInfo, Engine, create_args, hash_of};
use crate::errors::{Context, Error, Result};
use crate::pod::{self, Ensured, state};
use crate::project::{DependsCondition, Project, PullPolicy, RuntimeKind, Service};
use indexmap::{IndexMap, IndexSet};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct UpOptions {
    pub services: Vec<String>,
    pub force_recreate: bool,
    pub pull: Option<PullPolicy>,
    /// How long to wait for each depends_on condition.
    pub wait_timeout: Option<Duration>,
    /// Wait for every started service to be running or healthy.
    pub wait: bool,
}

pub struct Orchestrator {
    pub project: Project,
    engines: BTreeMap<RuntimeKind, Engine>,
}

impl Orchestrator {
    pub fn new(project: Project) -> Self {
        let mut engines = BTreeMap::new();
        for s in project.services.values() {
            engines
                .entry(s.runtime)
                .or_insert_with(|| Engine::new(s.runtime));
        }
        engines
            .entry(project.runtime)
            .or_insert_with(|| Engine::new(project.runtime));
        Self { project, engines }
    }

    pub fn engine(&self, kind: RuntimeKind) -> &Engine {
        // Every runtime the project uses was inserted in `new`.
        self.engines
            .get(&kind)
            .unwrap_or_else(|| unreachable!("engine {kind} not prepared"))
    }

    fn all_engines(&self) -> Vec<Engine> {
        // Also look at engines the project does not use (a service may have
        // moved runtimes since the last `up`).
        let mut v: Vec<Engine> = self.engines.values().cloned().collect();
        for k in [RuntimeKind::Docker, RuntimeKind::Podman, RuntimeKind::Kiri] {
            if !self.engines.contains_key(&k) {
                v.push(Engine::new(k));
            }
        }
        v
    }

    /// Every container of this project, across all engines that answer.
    pub async fn containers(&self) -> Vec<(Engine, ContainerInfo)> {
        let mut out = Vec::new();
        for e in self.all_engines() {
            let used = self.engines.contains_key(&e.kind);
            match e.list(&self.project.name).await {
                Ok(list) => out.extend(list.into_iter().map(|c| (e.clone(), c))),
                Err(err) if used => tracing::warn!("{err}"),
                Err(_) => {}
            }
        }
        out
    }

    /// The selected services plus everything they depend on, in start order.
    pub fn start_order(&self, selected: &[String]) -> Result<Vec<String>> {
        for s in selected {
            if !self.project.services.contains_key(s) {
                return Err(Error::config(format!("no such service {s:?}")));
            }
        }
        let roots: Vec<String> = if selected.is_empty() {
            self.project.services.keys().cloned().collect()
        } else {
            selected.to_vec()
        };
        let mut order: IndexSet<String> = IndexSet::new();
        fn visit(p: &Project, n: &str, order: &mut IndexSet<String>) {
            if order.contains(n) {
                return;
            }
            if let Some(s) = p.services.get(n) {
                for d in s.depends_on.keys() {
                    visit(p, d, order);
                }
            }
            order.insert(n.to_string());
        }
        for r in &roots {
            visit(&self.project, r, &mut order);
        }
        Ok(order.into_iter().collect())
    }

    pub async fn up(&self, opts: &UpOptions) -> Result<()> {
        let order = self.start_order(&opts.services)?;
        for e in self.engines.values() {
            if order
                .iter()
                .any(|s| self.project.services[s].runtime == e.kind)
            {
                e.check().await?;
            }
        }
        let _lock = lock(&self.project.name)?;

        // Images first, so a failed pull leaves nothing half started.
        for name in &order {
            let svc = &self.project.services[name];
            self.ensure_image(svc, opts.pull.unwrap_or(svc.pull_policy))
                .await?;
        }
        self.ensure_volumes(&order).await?;

        // Pods, then hosts files and firewalls once every address is known.
        let pods: IndexSet<String> = order
            .iter()
            .map(|s| self.project.services[s].pod.clone())
            .collect();
        let mut recreated: IndexSet<String> = IndexSet::new();
        let mut states = IndexMap::new();
        let existing = self.containers().await;
        for pod_name in &pods {
            let pod = &self.project.pods[pod_name];
            let hash = pod::config_hash(&self.project, pod);
            let changed = state::PodState::load(&self.project.name, pod_name)?
                .is_some_and(|s| s.config_hash != hash || !pod::ns::is_pinned(&s.netns));
            if changed {
                // Containers hold the old namespaces; remove them first.
                for (e, c) in existing.iter().filter(|(_, c)| &c.pod == pod_name) {
                    println!("Removing {} (pod {pod_name} changed)", c.name);
                    e.remove(&c.name).await?;
                }
            }
            let (st, how) = pod::ensure(&self.project, pod).await?;
            if how == Ensured::Created {
                println!("Pod {pod_name} created ({})", describe_pod(&st));
                recreated.insert(pod_name.clone());
            }
            states.insert(pod_name.clone(), st);
        }
        pod::refresh(&self.project)?;

        let mut started: Vec<String> = Vec::new();
        for name in &order {
            let svc = &self.project.services[name];
            for (dep, cond) in &svc.depends_on {
                self.wait_for(
                    dep,
                    *cond,
                    opts.wait_timeout.unwrap_or(Duration::from_secs(300)),
                )
                .await?;
            }
            let st = &states[&svc.pod];
            self.ensure_container(svc, st, opts.force_recreate).await?;
            started.push(name.clone());
        }
        if opts.wait {
            for name in &started {
                let svc = &self.project.services[name];
                let cond = if svc.healthcheck.as_ref().is_some_and(|h| !h.disabled) {
                    DependsCondition::ServiceHealthy
                } else {
                    DependsCondition::ServiceStarted
                };
                self.wait_for(
                    name,
                    cond,
                    opts.wait_timeout.unwrap_or(Duration::from_secs(300)),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn ensure_image(&self, svc: &Service, policy: PullPolicy) -> Result<()> {
        let e = self.engine(svc.runtime);
        match policy {
            PullPolicy::Always => e.pull(&svc.image).await,
            PullPolicy::Missing => {
                if e.image_exists(&svc.image).await {
                    Ok(())
                } else {
                    println!("Pulling {} for {} ({})", svc.image, svc.name, svc.runtime);
                    e.pull(&svc.image).await
                }
            }
            PullPolicy::Never => {
                if e.image_exists(&svc.image).await {
                    Ok(())
                } else {
                    Err(Error::engine(format!(
                        "{}: image {} is not present and pull_policy is never",
                        svc.name, svc.image
                    )))
                }
            }
        }
    }

    async fn ensure_volumes(&self, order: &[String]) -> Result<()> {
        let mut done: IndexSet<(RuntimeKind, String)> = IndexSet::new();
        for name in order {
            let svc = &self.project.services[name];
            for m in svc
                .mounts
                .iter()
                .filter(|m| m.kind == crate::project::MountKind::Volume && !m.source.is_empty())
            {
                if !done.insert((svc.runtime, m.source.clone())) {
                    continue;
                }
                let Some(v) = self
                    .project
                    .volumes
                    .values()
                    .find(|v| v.engine_name == m.source)
                else {
                    continue;
                };
                if v.external {
                    continue;
                }
                let opts: Vec<(String, String)> = v
                    .driver_opts
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                self.engine(svc.runtime)
                    .volume_ensure(
                        &v.engine_name,
                        v.driver.as_deref(),
                        &opts,
                        &self.project.name,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn ensure_container(
        &self,
        svc: &Service,
        st: &state::PodState,
        force: bool,
    ) -> Result<()> {
        let e = self.engine(svc.runtime);
        let image_cmd = if svc.runtime != RuntimeKind::Podman
            && svc.entrypoint.as_ref().is_some_and(|ep| ep.len() > 1)
            && svc.command.is_none()
        {
            Some(e.image_cmd(&svc.image).await?)
        } else {
            None
        };
        let args = create_args(&self.project, svc, st, image_cmd);
        let want = hash_of(&args);

        // The same container name on another engine (runtime changed).
        for other in self.all_engines().iter().filter(|o| o.kind != e.kind) {
            if let Ok(Some(c)) = other.inspect(&svc.container_name).await {
                if c.service == svc.name {
                    println!(
                        "Removing {} from {} (runtime is now {})",
                        c.name, other.kind, e.kind
                    );
                    other.remove(&c.name).await?;
                }
            }
        }

        if let Some(c) = e.inspect(&svc.container_name).await? {
            if c.hash == want && !force {
                if c.status != "running" {
                    println!("Starting {}", svc.container_name);
                    e.start(&svc.container_name).await?;
                } else {
                    println!("{} is up to date", svc.container_name);
                }
                return Ok(());
            }
            println!("Recreating {}", svc.container_name);
            let _ = e
                .stop(
                    &svc.container_name,
                    svc.stop_grace_period.map(|d| d.as_secs()),
                )
                .await;
            e.remove(&svc.container_name).await?;
        } else {
            println!(
                "Creating {} ({}, pod {})",
                svc.container_name, svc.runtime, svc.pod
            );
        }
        e.create(&args).await?;
        e.start(&svc.container_name).await
    }

    /// Waits until `service` satisfies `cond`.
    pub async fn wait_for(
        &self,
        service: &str,
        cond: DependsCondition,
        timeout: Duration,
    ) -> Result<()> {
        let svc = &self.project.services[service];
        let e = self.engine(svc.runtime);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut announced = false;
        loop {
            let info = e.inspect(&svc.container_name).await?;
            let ok = match (&info, cond) {
                (None, _) => false,
                (Some(c), DependsCondition::ServiceStarted) => {
                    c.status == "running" || c.status == "exited"
                }
                (Some(c), DependsCondition::ServiceHealthy) => {
                    if c.status == "exited" {
                        return Err(Error::engine(format!(
                            "{service} exited while waiting for it to become healthy"
                        )));
                    }
                    match c.health.as_deref() {
                        Some("healthy") => true,
                        Some(_) => false,
                        None => {
                            return Err(Error::engine(format!(
                                "{service} has no healthcheck, so service_healthy can never be met"
                            )));
                        }
                    }
                }
                (Some(c), DependsCondition::ServiceCompletedSuccessfully) => {
                    if c.status == "exited" {
                        match c.exit_code {
                            Some(0) => true,
                            code => {
                                return Err(Error::engine(format!(
                                    "{service} exited with code {}",
                                    code.unwrap_or(-1)
                                )));
                            }
                        }
                    } else {
                        false
                    }
                }
            };
            if ok {
                return Ok(());
            }
            if !announced {
                println!(
                    "Waiting for {service} ({})",
                    match cond {
                        DependsCondition::ServiceStarted => "started",
                        DependsCondition::ServiceHealthy => "healthy",
                        DependsCondition::ServiceCompletedSuccessfully => "completed",
                    }
                );
                announced = true;
            }
            if svc.runtime == RuntimeKind::Podman && cond == DependsCondition::ServiceHealthy {
                // Podman runs healthchecks from systemd timers when it can;
                // run one directly so waiting works everywhere.
                let _ = e.run(["healthcheck", "run", &svc.container_name]).await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::engine(format!("timed out waiting for {service}")));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn down(&self, volumes: bool, timeout: Option<u64>) -> Result<()> {
        let _lock = lock(&self.project.name)?;
        let containers = self.containers().await;
        // Reverse dependency order for known services, then the rest.
        let order = self.start_order(&[]).unwrap_or_default();
        let mut sorted: Vec<&(Engine, ContainerInfo)> = containers.iter().collect();
        sorted.sort_by_key(|(_, c)| {
            std::cmp::Reverse(
                order
                    .iter()
                    .position(|s| *s == c.service)
                    .unwrap_or(usize::MAX),
            )
        });
        for (e, c) in &sorted {
            println!("Stopping {}", c.name);
            let grace = self
                .project
                .services
                .get(&c.service)
                .and_then(|s| s.stop_grace_period)
                .map(|d| d.as_secs());
            let _ = e.stop(&c.name, timeout.or(grace)).await;
            e.remove(&c.name).await?;
        }
        let mut pods: IndexSet<String> = state::list_pods(&self.project.name).into_iter().collect();
        pods.extend(self.project.pods.keys().cloned());
        for p in &pods {
            if state::PodState::load(&self.project.name, p)?.is_some()
                || state::netns_path(&self.project.name, p).exists()
            {
                println!("Removing pod {p}");
                pod::remove(&self.project.name, p).await?;
            }
        }
        if volumes {
            for v in self.project.volumes.values().filter(|v| !v.external) {
                for e in self.engines.values() {
                    if e.volume_remove(&v.engine_name).await.is_ok() {
                        println!("Removed volume {}", v.engine_name);
                    }
                }
            }
        }
        let dir = state::project_dir(&self.project.name);
        let _ = std::fs::remove_file(dir.join("lock"));
        let _ = std::fs::remove_dir(dir.join("pods"));
        let _ = std::fs::remove_dir(&dir);
        Ok(())
    }

    pub async fn stop(&self, services: &[String], timeout: Option<u64>) -> Result<()> {
        let mut order = self.start_order(services)?;
        if !services.is_empty() {
            order.retain(|s| services.contains(s));
        }
        for name in order.iter().rev() {
            let svc = &self.project.services[name];
            let e = self.engine(svc.runtime);
            if e.inspect(&svc.container_name)
                .await?
                .is_some_and(|c| c.status == "running")
            {
                println!("Stopping {}", svc.container_name);
                e.stop(
                    &svc.container_name,
                    timeout.or(svc.stop_grace_period.map(|d| d.as_secs())),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub async fn start(&self, services: &[String]) -> Result<()> {
        let mut order = self.start_order(services)?;
        if !services.is_empty() {
            order.retain(|s| services.contains(s));
        }
        for name in &order {
            let svc = &self.project.services[name];
            let st = state::PodState::load(&self.project.name, &svc.pod)?
                .filter(|s| pod::ns::is_pinned(&s.netns))
                .ok_or_else(|| {
                    Error::State(format!("pod {} is not up; run `kirisame up`", svc.pod))
                })?;
            let e = self.engine(svc.runtime);
            match e.inspect(&svc.container_name).await? {
                Some(c) if c.pod_id != st.id => {
                    return Err(Error::State(format!(
                        "{} belongs to an older pod; run `kirisame up`",
                        c.name
                    )));
                }
                Some(c) if c.status == "running" => {}
                Some(_) => {
                    println!("Starting {}", svc.container_name);
                    e.start(&svc.container_name).await?;
                }
                None => {
                    return Err(Error::State(format!(
                        "{} does not exist; run `kirisame up`",
                        svc.container_name
                    )));
                }
            }
        }
        Ok(())
    }
}

pub fn describe_pod(st: &state::PodState) -> String {
    let mut parts = Vec::new();
    for i in &st.interfaces {
        let addrs: Vec<String> = i.addresses.iter().map(ToString::to_string).collect();
        let mut s = format!("{} on {}", i.ifname, i.network);
        if i.dhcp {
            let lease =
                pod::dhcp::LeaseFile::load(&state::pod_dir(&st.project, &st.name), &i.ifname)
                    .ok()
                    .flatten();
            match lease {
                Some(l) => s.push_str(&format!(" {} (dhcp)", l.address())),
                None => s.push_str(" (dhcp pending)"),
            }
        } else if !addrs.is_empty() {
            s.push_str(&format!(" {}", addrs.join(" ")));
        }
        parts.push(s);
    }
    if parts.is_empty() {
        "loopback only".into()
    } else {
        parts.join(", ")
    }
}

/// An exclusive lock on the project's run directory, held while a
/// mutating command runs.
pub struct Lock(#[allow(dead_code)] nix::fcntl::Flock<std::fs::File>);

pub fn lock(project: &str) -> Result<Lock> {
    let dir = state::project_dir(project);
    state::ensure_dir(&dir)?;
    let path = dir.join("lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ctx(|| format!("open {}", path.display()))?;
    match nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(l) => Ok(Lock(l)),
        Err((_, nix::Error::EWOULDBLOCK)) => Err(Error::State(format!(
            "another kirisame command is working on project {project}"
        ))),
        Err((_, e)) => Err(e).ctx(|| format!("lock {}", path.display())),
    }
}
