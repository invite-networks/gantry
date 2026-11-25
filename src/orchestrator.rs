//! The commands: bringing pods and their services up and down in order.

use crate::engine::exec::{RunRequest, oneoff_body};
use crate::engine::spec::grace_secs;
use crate::engine::{ContainerInfo, Engine, create_spec, hash_of};
use crate::errors::{Context, Error, Result};
use crate::plugin::{Hook, Plugins, Registry};
use crate::pod::{self, Ensured, state};
use crate::project::{DependsCondition, Project, PullPolicy, RuntimeKind, Service};
use futures_util::future::try_join_all;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
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
    /// The runtimes the project uses.
    engines: BTreeMap<RuntimeKind, Engine>,
    /// Every runtime, sharing connections with `engines`, for finding
    /// containers that moved between runtimes.
    all: Vec<Engine>,
    /// Installed plugins, started on the first hook that needs one.
    pub plugins: Plugins,
}

impl Orchestrator {
    pub fn new(project: Project) -> Self {
        Self::with_plugins(project, Arc::new(Registry::system()))
    }

    /// Like [`Orchestrator::new`], with the plugins to use.
    pub fn with_plugins(project: Project, registry: Arc<Registry>) -> Self {
        let mut engines = BTreeMap::new();
        for s in project.services.values() {
            engines
                .entry(s.runtime)
                .or_insert_with(|| Engine::new(s.runtime));
        }
        engines
            .entry(project.runtime)
            .or_insert_with(|| Engine::new(project.runtime));
        let all = [RuntimeKind::Docker, RuntimeKind::Kiri]
            .into_iter()
            .map(|k| engines.get(&k).cloned().unwrap_or_else(|| Engine::new(k)))
            .collect();
        let plugins = Plugins::new(registry, &project.name, project.plugins.clone());
        Self {
            project,
            engines,
            all,
            plugins,
        }
    }

    /// Tells the plugins subscribed to `hook` about the project as it is
    /// now. `extra` keys are added to the event data.
    pub async fn notify(&self, hook: Hook, extra: Option<Value>) {
        if !self.plugins.wants(hook) {
            return;
        }
        let all = self.containers().await.unwrap_or_else(|e| {
            tracing::warn!("plugins: list containers: {e}");
            vec![]
        });
        let mut data = self.plugin_data(&all, None);
        if let (Value::Object(d), Some(Value::Object(x))) = (&mut data, extra) {
            d.extend(x);
        }
        self.plugins.emit(hook, &data).await;
    }

    /// What plugins are told: each pod with its interfaces and containers.
    /// `state` replaces every pod and container state, as `down` does once
    /// they are gone.
    pub fn plugin_data(&self, all: &[(Engine, ContainerInfo)], state: Option<&str>) -> Value {
        let p = &self.project;
        let pods: Vec<Value> = p
            .pods
            .values()
            .map(|pod| {
                let st = state::PodState::load(&p.name, &pod.name).ok().flatten();
                let interfaces: Vec<Value> = pod
                    .attachments
                    .iter()
                    .map(|a| {
                        let live = st
                            .as_ref()
                            .and_then(|s| s.interfaces.iter().find(|i| i.ifname == a.ifname));
                        let configured = match &a.addressing {
                            crate::project::Addressing::Static(v) => v.clone(),
                            crate::project::Addressing::Dhcp => vec![],
                        };
                        let addresses = live.map_or(configured, |i| i.addresses.clone());
                        json!({
                            "name": a.ifname,
                            "parent": a.parent,
                            "driver": a.driver,
                            "dhcp": matches!(a.addressing, crate::project::Addressing::Dhcp),
                            "addresses": addresses,
                            "gateway": live.and_then(|i| i.gateway).or(a.gateway),
                            "gateway6": a.gateway6,
                            "default_route": a.default_route,
                            "mac": live.and_then(|i| i.mac.clone()),
                        })
                    })
                    .collect();
                let containers: Vec<Value> = all
                    .iter()
                    .filter(|(_, c)| c.project == p.name && !c.oneoff && c.pod == pod.name)
                    .map(|(e, c)| {
                        let svc = p.services.get(&c.service);
                        json!({
                            "service": c.service,
                            "name": c.name,
                            "id": c.id,
                            "engine": e.kind.as_str(),
                            "image": c.image,
                            "state": state.unwrap_or(&c.status),
                            "health": c.health,
                            "restart": svc.and_then(|s| s.restart.clone()),
                            "command": svc.and_then(|s| s.command.clone()),
                            "cpus": svc.and_then(|s| s.cpus),
                            "mem_limit": svc.and_then(|s| s.mem_limit),
                            "labels": svc.map(|s| s.labels.clone()).unwrap_or_default(),
                            "volumes": c.mounts.iter().map(|m| json!({
                                "type": m.kind,
                                "name": m.name,
                                "source": m.source,
                                "target": m.destination,
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let pod_state =
                    state.unwrap_or(if containers.iter().any(|c| c["state"] == "running") {
                        "running"
                    } else if st.is_some() {
                        "created"
                    } else {
                        "down"
                    });
                json!({
                    "name": pod.name,
                    "netns": state::netns_name(&p.name, &pod.name),
                    "hostname": pod.hostname,
                    "state": pod_state,
                    "interfaces": interfaces,
                    "containers": containers,
                })
            })
            .collect();
        json!({
            "project": p.name,
            "dir": p.dir,
            "pods": pods,
        })
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
        self.all.clone()
    }

    /// Every container of this project, across all engines, queried in
    /// parallel. An engine the project uses must answer; engines it does
    /// not use are skipped when they are down.
    pub async fn containers(&self) -> Result<Vec<(Engine, ContainerInfo)>> {
        let engines = self.all_engines();
        let name = &self.project.name;
        let results = futures_util::future::join_all(engines.iter().map(|e| async move {
            let t = std::time::Instant::now();
            let r = e.list(name).await;
            tracing::debug!("{} list: {:?}", e.kind, t.elapsed());
            r
        }))
        .await;
        let mut out = Vec::new();
        for (e, r) in engines.into_iter().zip(results) {
            let used = self.engines.contains_key(&e.kind);
            match r {
                Ok(list) => out.extend(list.into_iter().map(|c| (e.clone(), c))),
                Err(err) if used => return Err(err),
                Err(_) => {}
            }
        }
        Ok(out)
    }

    /// Whether a pod exists and its shape differs from the project (or its
    /// namespace is gone), so that `ensure_pods` will recreate it.
    fn pod_changed(&self, pod_name: &str) -> Result<bool> {
        let Some(pod) = self.project.pods.get(pod_name) else {
            return Ok(false);
        };
        let hash = pod::config_hash(&self.project, pod);
        Ok(state::PodState::load(&self.project.name, pod_name)?
            .is_some_and(|s| s.config_hash != hash || !pod::ns::is_pinned(&s.netns)))
    }

    /// The services to bring up: the selection and its dependencies, plus
    /// every service of a selected pod that is about to be recreated, since
    /// recreating a pod removes all of its containers.
    fn up_order(&self, selected: &[String]) -> Result<Vec<String>> {
        let order = self.start_order(selected)?;
        if selected.is_empty() {
            return Ok(order);
        }
        let mut changed: IndexSet<String> = IndexSet::new();
        for name in &order {
            let pod = &self.project.services[name].pod;
            if self.pod_changed(pod)? {
                changed.insert(pod.clone());
            }
        }
        if changed.is_empty() {
            return Ok(order);
        }
        let mut wanted: Vec<String> = order.clone();
        for (name, svc) in &self.project.services {
            if changed.contains(&svc.pod) && !wanted.contains(name) {
                println!("Including {name}: pod {} is being recreated", svc.pod);
                wanted.push(name.clone());
            }
        }
        self.start_order(&wanted)
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
        let order = self.up_order(&opts.services)?;
        let _lock = lock(&self.project.name)?;
        guard_project_name(&self.project)?;
        guard_addresses(&self.project)?;
        crate::datamove::refuse_unmoved(self, &order).await?;
        self.notify(Hook::PreUp, None).await;
        self.up_locked(opts).await?;
        self.notify(Hook::PostUp, None).await;
        Ok(())
    }

    /// `up` for a caller that already holds the project lock and has done
    /// its own mount checks (`gantry data move`).
    pub async fn up_locked(&self, opts: &UpOptions) -> Result<()> {
        let order = self.up_order(&opts.services)?;
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

        // Images first, so a failed pull leaves nothing half started.
        self.ensure_images(&order, opts.pull, opts.build).await?;
        self.ensure_volumes(&order).await?;

        // Pods, then hosts files and firewalls once every address is known.
        let pods: IndexSet<String> = order
            .iter()
            .map(|s| self.project.services[s].pod.clone())
            .collect();
        let all = self.containers().await?;
        let states = self.ensure_pods(&pods, &all).await?;
        let existing: Vec<(Engine, ContainerInfo)> =
            all.into_iter().filter(|(_, c)| !c.oneoff).collect();

        let timeout = opts.wait_timeout.unwrap_or(Duration::from_secs(300));
        let states = &states;
        let existing = &existing;
        for level in self.levels(&order) {
            try_join_all(level.iter().map(|name| async move {
                let svc = &self.project.services[name];
                for (dep, cond) in &svc.depends_on {
                    self.wait_for(dep, *cond, timeout).await?;
                }
                self.ensure_container(svc, &states[&svc.pod], opts.force_recreate, existing)
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

    /// Creates or keeps the given pods, then refreshes every pod's hosts
    /// file and firewall. Containers of a pod whose shape changed are
    /// removed first, one-off containers included, since they hold the old
    /// namespaces.
    pub async fn ensure_pods(
        &self,
        pods: &IndexSet<String>,
        existing: &[(Engine, ContainerInfo)],
    ) -> Result<IndexMap<String, state::PodState>> {
        let mut states = IndexMap::new();
        for pod_name in pods {
            let pod = self
                .project
                .pods
                .get(pod_name)
                .ok_or_else(|| Error::config(format!("no such pod {pod_name:?}")))?;
            if self.pod_changed(pod_name)? {
                try_join_all(existing.iter().filter(|(_, c)| &c.pod == pod_name).map(
                    |(e, c)| async move {
                        println!("Removing {} (pod {pod_name} changed)", c.name);
                        e.remove(&c.name).await
                    },
                ))
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
        Ok(states)
    }

    /// Runs a one-off container of `service` in its pod (`gantry run`)
    /// and returns its exit code. Dependencies are brought up first unless
    /// `no_deps` is set.
    pub async fn run_oneoff(
        &self,
        service: &str,
        req: &RunRequest,
        name: Option<String>,
        rm: bool,
        no_deps: bool,
    ) -> Result<i64> {
        let svc = self
            .project
            .services
            .get(service)
            .ok_or_else(|| Error::config(format!("no such service {service:?}")))?;
        if !no_deps && !svc.depends_on.is_empty() {
            let deps: Vec<String> = svc.depends_on.keys().cloned().collect();
            self.up(&UpOptions {
                services: deps.clone(),
                ..Default::default()
            })
            .await?;
            for (dep, cond) in &svc.depends_on {
                self.wait_for(dep, *cond, Duration::from_secs(300)).await?;
            }
        }
        let e = self.engine(svc.runtime);
        e.check().await?;
        let st = {
            let _lock = lock(&self.project.name)?;
            self.ensure_images(&[service.to_string()], None, false)
                .await?;
            if self.pod_changed(&svc.pod)? {
                return Err(Error::State(format!(
                    "pod {} has changed; run `gantry up` before `run`",
                    svc.pod
                )));
            }
            let all = self.containers().await?;
            let pods: IndexSet<String> = [svc.pod.clone()].into_iter().collect();
            let mut states = self.ensure_pods(&pods, &all).await?;
            states
                .shift_remove(&svc.pod)
                .ok_or_else(|| Error::State(format!("pod {} did not come up", svc.pod)))?
        };
        let body = oneoff_body(create_spec(&self.project, svc, &st, None), req);
        let name = name.unwrap_or_else(|| {
            format!(
                "{}-{}-run-{}",
                self.project.name,
                svc.name,
                crate::util::random_id(3)
            )
        });
        e.run_attached(&name, body, svc.platform.as_deref(), req, rm)
            .await
    }

    /// Sends a signal to the selected services' containers.
    pub async fn kill(&self, services: &[String], signal: &str) -> Result<()> {
        let order = self.selected(services)?;
        try_join_all(order.iter().map(|name| async move {
            let svc = &self.project.services[name];
            if self.owned(svc).await?.is_none() {
                return Ok(());
            }
            println!("Killing {}", svc.container_name);
            self.engine(svc.runtime)
                .kill(&svc.container_name, signal)
                .await
        }))
        .await?;
        Ok(())
    }

    /// Removes the selected services' stopped containers (all of them
    /// after stopping when `stop` is set). Pods stay.
    pub async fn rm(&self, services: &[String], stop: bool) -> Result<()> {
        if stop {
            self.stop(services, None).await?;
        }
        let order = self.selected(services)?;
        try_join_all(order.iter().map(|name| async move {
            let svc = &self.project.services[name];
            let e = self.engine(svc.runtime);
            match self.owned(svc).await? {
                Some(c) if matches!(c.status.as_str(), "exited" | "created" | "dead") => {
                    println!("Removing {}", c.name);
                    e.remove(&c.name).await
                }
                Some(c) => {
                    println!("{} is {}; stop it first or pass --stop", c.name, c.status);
                    Ok::<(), Error>(())
                }
                None => Ok(()),
            }
        }))
        .await?;
        Ok(())
    }

    /// The service's container, if one exists under its name. A container
    /// with that name that belongs to something else is an error: Gantry
    /// never stops or removes what it did not create.
    pub async fn owned(&self, svc: &Service) -> Result<Option<ContainerInfo>> {
        match self
            .engine(svc.runtime)
            .inspect(&svc.container_name)
            .await?
        {
            Some(c) if c.project != self.project.name => Err(Error::State(format!(
                "container name {} is taken by a container that is not part of project {} (project label {:?}); remove it or set container_name",
                svc.container_name, self.project.name, c.project
            ))),
            other => Ok(other),
        }
    }

    /// The named services, or all of them, checked to exist.
    pub fn selected(&self, services: &[String]) -> Result<Vec<String>> {
        if services.is_empty() {
            return Ok(self.project.services.keys().cloned().collect());
        }
        for s in services {
            if !self.project.services.contains_key(s) {
                return Err(Error::config(format!("no such service {s:?}")));
            }
        }
        Ok(services.to_vec())
    }

    /// Builds or pulls what the services need. Pulls run in parallel;
    /// builds run one at a time so their output stays readable.
    pub async fn ensure_images(
        &self,
        order: &[String],
        pull: Option<PullPolicy>,
        force_build: bool,
    ) -> Result<()> {
        let mut seen: IndexSet<(RuntimeKind, String, bool)> = IndexSet::new();
        let mut builds: Vec<&Service> = Vec::new();
        let mut pulls: Vec<&Service> = Vec::new();
        for name in order {
            let svc = &self.project.services[name];
            if !seen.insert((svc.runtime, svc.image.clone(), svc.build.is_some())) {
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
            self.engine(svc.runtime)
                .pull(&svc.image, svc.platform.as_deref())
                .await?;
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
        existing: &[(Engine, ContainerInfo)],
    ) -> Result<()> {
        let e = self.engine(svc.runtime);
        let image_id = e.image_id(&image_ref(svc)).await?;
        let body = create_spec(&self.project, svc, st, image_id.as_deref());
        let want = hash_of(&body);

        // The same container name on another engine (runtime changed).
        for (other, c) in existing
            .iter()
            .filter(|(o, c)| o.kind != e.kind && c.service == svc.name)
        {
            println!(
                "Removing {} from {} (runtime is now {})",
                c.name, other.kind, e.kind
            );
            other.remove(&c.name).await?;
        }

        if let Some(c) = self.owned(svc).await? {
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
                .stop(&svc.container_name, svc.stop_grace_period.map(grace_secs))
                .await;
            e.remove(&svc.container_name).await?;
        } else {
            println!(
                "Creating {} ({}, pod {})",
                svc.container_name, svc.runtime, svc.pod
            );
        }
        e.create(&svc.container_name, body, svc.platform.as_deref())
            .await?;
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
        let deadline = tokio::time::Instant::now() + timeout;
        let mut announced = false;
        loop {
            let info = self.owned(svc).await?;
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
            if now >= deadline {
                return Err(Error::engine(format!("timed out waiting for {service}")));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn down(&self, volumes: bool, timeout: Option<u64>) -> Result<()> {
        let _lock = lock(&self.project.name)?;
        let containers = self.containers().await?;
        // Pod state is gone once down finishes, so post_down's data is
        // taken now.
        let gone = self
            .plugins
            .wants(Hook::PostDown)
            .then(|| self.plugin_data(&containers, Some("down")));
        if self.plugins.wants(Hook::PreDown) {
            let data = self.plugin_data(&containers, None);
            self.plugins.emit(Hook::PreDown, &data).await;
        }
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
                .map(grace_secs);
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
        // The lock file stays: unlinking it while held would let a second
        // command lock a new file at the same path.
        let _ = std::fs::remove_file(Project::snapshot_path(&self.project.name));
        let _ = std::fs::remove_dir(dir.join("pods"));
        let _ = std::fs::remove_dir(&dir);
        if let Some(data) = gone {
            self.plugins.emit(Hook::PostDown, &data).await;
        }
        Ok(())
    }

    pub async fn stop(&self, services: &[String], timeout: Option<u64>) -> Result<()> {
        self.stop_services(services, timeout).await?;
        self.notify(Hook::PostStop, None).await;
        Ok(())
    }

    pub async fn start(&self, services: &[String]) -> Result<()> {
        self.start_services(services).await?;
        self.notify(Hook::PostStart, None).await;
        Ok(())
    }

    /// Stops then starts the services; plugins hear one `post_restart`.
    pub async fn restart(&self, services: &[String], timeout: Option<u64>) -> Result<()> {
        self.stop_services(services, timeout).await?;
        self.start_services(services).await?;
        self.notify(Hook::PostRestart, None).await;
        Ok(())
    }

    async fn stop_services(&self, services: &[String], timeout: Option<u64>) -> Result<()> {
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
                if self
                    .owned(svc)
                    .await?
                    .is_some_and(|c| c.status == "running")
                {
                    println!("Stopping {}", svc.container_name);
                    e.stop(
                        &svc.container_name,
                        timeout.or(svc.stop_grace_period.map(grace_secs)),
                    )
                    .await?;
                }
                Ok::<(), Error>(())
            }))
            .await?;
        }
        Ok(())
    }

    async fn start_services(&self, services: &[String]) -> Result<()> {
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
        match self.owned(svc).await? {
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

/// The image reference to check for a service.
fn image_ref(svc: &Service) -> String {
    crate::engine::spec::service_image(svc)
}

pub fn describe_pod(st: &state::PodState) -> String {
    let mut parts = Vec::new();
    for i in &st.interfaces {
        let addrs: Vec<String> = i.addresses.iter().map(ToString::to_string).collect();
        let mut s = format!("{} on {}", i.ifname, i.parent);
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

/// Refuses a project whose name is already running from another
/// directory: Gantry keys pods and state by project name, so the two would
/// take over each other's pods.
pub fn guard_project_name(project: &Project) -> Result<()> {
    if state::list_pods(&project.name).is_empty() {
        return Ok(());
    }
    let Ok(bytes) = std::fs::read(Project::snapshot_path(&project.name)) else {
        return Ok(());
    };
    let Some(other) = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("dir")
                .and_then(|d| d.as_str())
                .map(std::path::PathBuf::from)
        })
    else {
        return Ok(());
    };
    let same = match (other.canonicalize(), project.dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => other == project.dir,
    };
    if same {
        return Ok(());
    }
    Err(Error::State(format!(
        "project {} is already running from {}; this one is in {}. Give it a different name: in its project file",
        project.name,
        other.display(),
        project.dir.display()
    )))
}

/// Refuses static addresses that a pod of another project on this host
/// holds (static or leased): both would answer on the LAN.
pub fn guard_addresses(project: &Project) -> Result<()> {
    let mut mine: IndexMap<std::net::IpAddr, &str> = IndexMap::new();
    for pod in project.pods.values() {
        for a in &pod.attachments {
            if let crate::project::Addressing::Static(addrs) = &a.addressing {
                for n in addrs {
                    mine.insert(n.addr(), &pod.name);
                }
            }
        }
    }
    if mine.is_empty() {
        return Ok(());
    }
    for other in state::list_projects() {
        if other == project.name {
            continue;
        }
        for pod in state::list_pods(&other) {
            let Some(st) = state::PodState::load(&other, &pod)? else {
                continue;
            };
            for n in st.addresses() {
                if let Some(mine_pod) = mine.get(&n.addr()) {
                    return Err(Error::config(format!(
                        "pod {mine_pod}: address {} is in use by pod {pod} of project {other} on this host",
                        n.addr()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// An exclusive lock on the project's run directory, held while a
/// mutating command runs.
pub struct Lock(#[allow(dead_code)] nix::fcntl::Flock<std::fs::File>);

pub fn lock(project: &str) -> Result<Lock> {
    // Lock files live outside the project directory so `down` can remove
    // that directory while holding the lock, and are never unlinked.
    let dir = state::run_dir().join("locks");
    state::ensure_dir(&dir)?;
    let path = dir.join(format!("{project}.lock"));
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

/// Tells plugins that the health watcher restarted a container, with the
/// project as `up` last recorded it.
pub async fn notify_heal_restart(action: crate::heal::HealAction) {
    let project = match Project::load_snapshot(&action.project) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("plugins: heal_restart: {}: {e}", action.project);
            return;
        }
    };
    let o = Orchestrator::new(project);
    o.notify(Hook::HealRestart, Some(json!({ "heal": action })))
        .await;
}
