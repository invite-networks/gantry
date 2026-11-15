//! The commands: bringing pods and their services up and down in order.

use crate::engine::{ContainerInfo, Engine, create_spec, hash_of};
use crate::errors::{Context, Error, Result};
use crate::pod::{self, Ensured, state};
use crate::project::{DependsCondition, Project, PullPolicy, RuntimeKind, Service};
use futures_util::future::try_join_all;
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
    /// Build images even when they exist.
    pub build: bool,
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

    /// Groups services into dependency levels: every service's
    /// dependencies sit in an earlier level, so one level can run in
    /// parallel.
    pub fn levels(&self, order: &[String]) -> Vec<Vec<String>> {
        let mut level: IndexMap<String, usize> = IndexMap::new();
        for name in order {
            let l = self
                .project
                .services
                .get(name)
                .map(|s| {
                    s.depends_on
                        .keys()
                        .filter_map(|d| level.get(d).map(|l| l + 1))
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            level.insert(name.clone(), l);
        }
        let depth = level.values().max().map_or(0, |m| m + 1);
        let mut out = vec![Vec::new(); depth];
        for (name, l) in level {
            if let Some(v) = out.get_mut(l) {
                v.push(name);
            }
        }
        out
    }

    pub async fn up(&self, opts: &UpOptions) -> Result<()> {
        let order = self.start_order(&opts.services)?;
        let used: Vec<&Engine> = self
            .engines
            .values()
            .filter(|e| {
                order
                    .iter()
                    .any(|s| self.project.services[s].runtime == e.kind)
            })
            .collect();
        try_join_all(used.iter().map(|e| e.check())).await?;
        let _lock = lock(&self.project.name)?;

        // Images first, so a failed pull leaves nothing half started.
        self.ensure_images(&order, opts.pull, opts.build).await?;
        self.ensure_volumes(&order).await?;

        // Pods, then hosts files and firewalls once every address is known.
        let pods: IndexSet<String> = order
            .iter()
            .map(|s| self.project.services[s].pod.clone())
            .collect();
        let mut states = IndexMap::new();
        let existing = self.containers().await;
        for pod_name in &pods {
            let pod = &self.project.pods[pod_name];
            let hash = pod::config_hash(&self.project, pod);
            let changed = state::PodState::load(&self.project.name, pod_name)?
                .is_some_and(|s| s.config_hash != hash || !pod::ns::is_pinned(&s.netns));
            if changed {
                // Containers hold the old namespaces; remove them first.
                let stale: Vec<&(Engine, ContainerInfo)> = existing
                    .iter()
                    .filter(|(_, c)| &c.pod == pod_name)
                    .collect();
                try_join_all(stale.iter().map(|(e, c)| async move {
                    println!("Removing {} (pod {pod_name} changed)", c.name);
                    e.remove(&c.name).await
                }))
                .await?;
            }
            let (st, how) = pod::ensure(&self.project, pod).await?;
            if how == Ensured::Created {
                println!("Pod {pod_name} created ({})", describe_pod(&st));
            }
            states.insert(pod_name.clone(), st);
        }
        self.project.save_snapshot()?;
        pod::refresh(&self.project)?;

        let timeout = opts.wait_timeout.unwrap_or(Duration::from_secs(300));
        let states = &states;
        for level in self.levels(&order) {
            try_join_all(level.iter().map(|name| async move {
                let svc = &self.project.services[name];
                for (dep, cond) in &svc.depends_on {
                    self.wait_for(dep, *cond, timeout).await?;
                }
                self.ensure_container(svc, &states[&svc.pod], opts.force_recreate)
                    .await
            }))
            .await?;
        }
        if opts.wait {
            try_join_all(order.iter().map(|name| {
                let svc = &self.project.services[name];
                let cond = if svc.healthcheck.as_ref().is_some_and(|h| !h.disabled) {
                    DependsCondition::ServiceHealthy
                } else {
                    DependsCondition::ServiceStarted
                };
                self.wait_for(name, cond, timeout)
            }))
            .await?;
        }
        Ok(())
    }

    /// Builds or pulls what the services need. Pulls run in parallel;
    /// builds run one at a time so their output stays readable.
    pub async fn ensure_images(
        &self,
        order: &[String],
        pull: Option<PullPolicy>,
        force_build: bool,
    ) -> Result<()> {
        let mut seen: IndexSet<(RuntimeKind, String)> = IndexSet::new();
        let mut builds: Vec<&Service> = Vec::new();
        let mut pulls: Vec<&Service> = Vec::new();
        for name in order {
            let svc = &self.project.services[name];
            if !seen.insert((svc.runtime, svc.image.clone())) {
                continue;
            }
            let e = self.engine(svc.runtime);
            let policy = pull.unwrap_or(svc.pull_policy);
            if svc.build.is_some() {
                let present = e.image_exists(&image_ref(svc)).await?;
                if force_build || policy == PullPolicy::Build || !present {
                    builds.push(svc);
                }
                continue;
            }
            match policy {
                PullPolicy::Always => pulls.push(svc),
                PullPolicy::Missing | PullPolicy::Build => {
                    if !e.image_exists(&svc.image).await? {
                        pulls.push(svc);
                    }
                }
                PullPolicy::Never => {
                    if !e.image_exists(&svc.image).await? {
                        return Err(Error::engine(format!(
                            "{}: image {} is not present and pull_policy is never",
                            svc.name, svc.image
                        )));
                    }
                }
            }
        }
        try_join_all(pulls.iter().map(|svc| async move {
            println!("Pulling {} for {} ({})", svc.image, svc.name, svc.runtime);
            let t = std::time::Instant::now();
            self.engine(svc.runtime).pull(&svc.image).await?;
            println!("Pulled {} in {:.1}s", svc.image, t.elapsed().as_secs_f32());
            Ok::<(), Error>(())
        }))
        .await?;
        for svc in builds {
            println!("Building {} for {} ({})", svc.image, svc.name, svc.runtime);
            self.engine(svc.runtime)
                .build(svc, &self.project.name)
                .await?;
        }
        Ok(())
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
        let body = create_spec(&self.project, svc, st);
        let want = hash_of(&body);

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
        e.create(&svc.container_name, body).await?;
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
        let mut last_check = None::<tokio::time::Instant>;
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
            let now = tokio::time::Instant::now();
            if cond == DependsCondition::ServiceHealthy
                && last_check.is_none_or(|t| now - t >= Duration::from_secs(1))
            {
                // Podman runs healthchecks from systemd timers when it can;
                // run one directly so waiting works everywhere.
                let _ = e.run_healthcheck(&svc.container_name).await;
                last_check = Some(now);
            }
            if now >= deadline {
                return Err(Error::engine(format!("timed out waiting for {service}")));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn down(&self, volumes: bool, timeout: Option<u64>) -> Result<()> {
        let _lock = lock(&self.project.name)?;
        let containers = self.containers().await;
        // Reverse dependency levels; containers of unknown services first.
        let order = self.start_order(&[]).unwrap_or_default();
        let mut groups: Vec<Vec<String>> = self.levels(&order);
        groups.reverse();
        let known: IndexSet<&String> = order.iter().collect();
        let unknown: Vec<&(Engine, ContainerInfo)> = containers
            .iter()
            .filter(|(_, c)| !known.contains(&c.service))
            .collect();
        let stop_rm = |e: &Engine, c: &ContainerInfo| {
            let grace = self
                .project
                .services
                .get(&c.service)
                .and_then(|s| s.stop_grace_period)
                .map(|d| d.as_secs());
            let (e, name) = (e.clone(), c.name.clone());
            async move {
                println!("Stopping {name}");
                let _ = e.stop(&name, timeout.or(grace)).await;
                e.remove(&name).await
            }
        };
        try_join_all(unknown.iter().map(|(e, c)| stop_rm(e, c))).await?;
        for group in groups {
            try_join_all(
                containers
                    .iter()
                    .filter(|(_, c)| group.contains(&c.service))
                    .map(|(e, c)| stop_rm(e, c)),
            )
            .await?;
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
        let _ = std::fs::remove_file(Project::snapshot_path(&self.project.name));
        let _ = std::fs::remove_dir(dir.join("pods"));
        let _ = std::fs::remove_dir(&dir);
        Ok(())
    }

    pub async fn stop(&self, services: &[String], timeout: Option<u64>) -> Result<()> {
        let mut order = self.start_order(services)?;
        if !services.is_empty() {
            order.retain(|s| services.contains(s));
        }
        let mut levels = self.levels(&order);
        levels.reverse();
        for level in levels {
            try_join_all(level.iter().map(|name| async move {
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
                Ok::<(), Error>(())
            }))
            .await?;
        }
        Ok(())
    }

    pub async fn start(&self, services: &[String]) -> Result<()> {
        let mut order = self.start_order(services)?;
        if !services.is_empty() {
            order.retain(|s| services.contains(s));
        }
        for level in self.levels(&order) {
            try_join_all(level.iter().map(|name| self.start_one(name))).await?;
        }
        Ok(())
    }

    async fn start_one(&self, name: &str) -> Result<()> {
        let svc = &self.project.services[name];
        let st = state::PodState::load(&self.project.name, &svc.pod)?
            .filter(|s| pod::ns::is_pinned(&s.netns))
            .ok_or_else(|| Error::State(format!("pod {} is not up; run `gantry up`", svc.pod)))?;
        let e = self.engine(svc.runtime);
        match e.inspect(&svc.container_name).await? {
            Some(c) if c.pod_id != st.id => Err(Error::State(format!(
                "{} belongs to an older pod; run `gantry up`",
                c.name
            ))),
            Some(c) if c.status == "running" => Ok(()),
            Some(_) => {
                println!("Starting {}", svc.container_name);
                e.start(&svc.container_name).await
            }
            None => Err(Error::State(format!(
                "{} does not exist; run `gantry up`",
                svc.container_name
            ))),
        }
    }
}

/// The image reference to check for a service (built Podman images live
/// under `localhost/`).
fn image_ref(svc: &Service) -> String {
    crate::engine::spec::service_image(svc)
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
            "another gantry command is working on project {project}"
        ))),
        Err((_, e)) => Err(e).ctx(|| format!("lock {}", path.display())),
    }
}
