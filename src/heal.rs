//! Self-healing: acting on containers their engine reports as unhealthy.
//!
//! Docker and kiri run healthchecks and mark a failing container
//! `unhealthy`, but neither restarts it; restart policies only cover
//! containers that exit. A service opts in with `health_on_failure:
//! restart` or `stop`, which Gantry records as a container label. [`watch`]
//! polls each engine for running containers with that label and acts on
//! the unhealthy ones.
//!
//! The watcher only ever acts on a container that is running and
//! unhealthy, so containers stopped on purpose are left alone. Before
//! acting it takes the project's command lock (skipping the project while
//! a `gantry` command such as `up`, `down` or `gantry boot start` holds
//! it) and inspects the container again under that lock.
//!
//! One watcher runs per host: [`watch`] holds an exclusive lock on
//! `/run/gantry/heal.lock` for as long as it runs.

use crate::engine::{ContainerInfo, Engine};
use crate::errors::{Context, Error, Result};
use crate::orchestrator::lock;
use crate::pod::state;
use crate::project::{HealthOnFailure, RuntimeKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Limits on how often the watcher restarts one container.
#[derive(Debug, Clone)]
pub struct Policy {
    /// How often each engine is checked.
    pub poll: Duration,
    /// Restarts allowed within `window` before the watcher gives up on a
    /// container.
    pub max_restarts: u32,
    pub window: Duration,
    /// The wait before the second restart; it doubles after each restart
    /// up to `backoff_max`. The first restart is immediate.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(10),
            max_restarts: 5,
            window: Duration::from_secs(30 * 60),
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(5 * 60),
        }
    }
}

impl Policy {
    /// The wait before restart number `done + 1`, given `done` restarts
    /// within the window.
    fn delay(&self, done: u32) -> Duration {
        if done == 0 {
            return Duration::ZERO;
        }
        let factor = 1u32.checked_shl(done - 1).unwrap_or(u32::MAX);
        self.backoff_base
            .checked_mul(factor)
            .unwrap_or(self.backoff_max)
            .min(self.backoff_max)
    }
}

/// What the watcher did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealKind {
    /// The container was restarted.
    Restarted,
    /// The container was stopped (`health_on_failure: stop`).
    Stopped,
    /// The container is unhealthy but the next restart is waiting on the
    /// backoff. Reported once per wait.
    BackingOff,
    /// The container used up its restarts within the window and is left
    /// running and unhealthy until it turns healthy or is recreated.
    GaveUp,
    /// The restart or stop call failed; `error` says why.
    Failed,
}

/// One action, as reported to the caller of [`watch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealAction {
    pub project: String,
    pub pod: String,
    pub service: String,
    pub container: String,
    pub container_id: String,
    pub engine: RuntimeKind,
    pub kind: HealKind,
    /// The restart this action is about, counted within the window (1 for
    /// the first). For `gave_up`, the restarts already made.
    pub attempt: u32,
    /// `Policy::max_restarts` (1 for `stop`).
    pub cap: u32,
    /// For `backing_off`, seconds until the next restart is allowed.
    pub next_delay_secs: Option<u64>,
    /// What triggered it, such as `unhealthy (failing streak 3)`.
    pub trigger: String,
    /// For `failed`, the engine's error.
    pub error: Option<String>,
    /// When it happened: milliseconds since the Unix epoch (UTC).
    pub at_ms: u64,
}

/// Pauses and resumes the watcher per project, from any task. A paused
/// project is never acted on. Clones share state.
#[derive(Debug, Clone, Default)]
pub struct Control {
    paused: Arc<Mutex<HashSet<String>>>,
    /// Held by the watcher from its last pause check through the restart
    /// or stop it makes, so `pause` can wait for that action to finish.
    acting: Arc<tokio::sync::Mutex<()>>,
}

impl Control {
    /// Pauses the project. When this returns, no action on the project is
    /// in progress and none will start until `resume`: it waits for an
    /// action already under way (at most one restart or stop) to finish.
    pub async fn pause(&self, project: &str) {
        if let Ok(mut p) = self.paused.lock() {
            p.insert(project.to_string());
        }
        drop(self.acting.lock().await);
    }
    pub fn resume(&self, project: &str) {
        if let Ok(mut p) = self.paused.lock() {
            p.remove(project);
        }
    }
    pub fn is_paused(&self, project: &str) -> bool {
        self.paused
            .lock()
            .map(|p| p.contains(project))
            .unwrap_or(false)
    }
}

/// The host-wide watcher lock, held for as long as [`watch`] runs.
pub struct HostLock(#[allow(dead_code)] nix::fcntl::Flock<std::fs::File>);

/// Path of the host-wide watcher lock.
pub fn lock_path() -> std::path::PathBuf {
    state::run_dir().join("heal.lock")
}

/// Takes the host-wide watcher lock, or fails when another watcher (such
/// as `gantry-supervise.service` or gantry-agent) already runs.
pub fn host_lock() -> Result<HostLock> {
    state::ensure_dir(&state::run_dir())?;
    let path = lock_path();
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ctx(|| format!("open {}", path.display()))?;
    match nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(l) => Ok(HostLock(l)),
        Err((_, nix::Error::EWOULDBLOCK)) => Err(Error::State(format!(
            "another health watcher is running on this host ({} is locked)",
            path.display()
        ))),
        Err((_, e)) => Err(e).ctx(|| format!("lock {}", path.display())),
    }
}

/// Restart history of one container.
#[derive(Debug, Default)]
struct Track {
    restarts: VecDeque<Instant>,
    gave_up: bool,
    /// The backoff wait already reported, so it is reported once.
    reported_wait: Option<Instant>,
    seen: bool,
}

/// Watches every engine in `runtimes` until `cancel` fires, then returns
/// once any restart in progress has finished. Takes the host lock first
/// and fails if another watcher holds it. Actions go to `actions` without
/// waiting: when the channel is full or closed the action is logged and
/// dropped. Engines that are not reachable are skipped and retried each
/// poll.
pub async fn watch(
    runtimes: &[RuntimeKind],
    policy: Policy,
    control: Control,
    actions: Option<mpsc::Sender<HealAction>>,
    cancel: CancellationToken,
) -> Result<()> {
    let _host = host_lock()?;
    let engines: Vec<Engine> = runtimes.iter().map(|k| Engine::new(*k)).collect();
    let mut tracks: HashMap<String, Track> = HashMap::new();
    let mut down: HashSet<RuntimeKind> = HashSet::new();
    tracing::info!(
        "health watcher started ({})",
        runtimes
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    loop {
        for t in tracks.values_mut() {
            t.seen = false;
        }
        for e in &engines {
            if cancel.is_cancelled() {
                break;
            }
            let list = match e.list_healing().await {
                Ok(l) => {
                    if down.remove(&e.kind) {
                        tracing::info!("{}: reachable again", e.kind);
                    }
                    l
                }
                Err(err) => {
                    if down.insert(e.kind) {
                        tracing::info!("{}: skipped while unreachable: {err}", e.kind);
                    }
                    continue;
                }
            };
            for c in list {
                if cancel.is_cancelled() {
                    break;
                }
                let t = tracks.entry(c.id.clone()).or_default();
                t.seen = true;
                if let Some(a) = step(e, &c, t, &policy, &control).await {
                    emit(&actions, a);
                }
            }
        }
        // Forget containers that are gone, or stopped, once nothing in
        // their history matters any more.
        let now = Instant::now();
        tracks.retain(|_, t| {
            t.seen
                || t.restarts
                    .back()
                    .is_some_and(|r| now.duration_since(*r) < policy.window)
        });
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(policy.poll) => {}
        }
    }
    tracing::info!("health watcher stopped");
    Ok(())
}

fn emit(actions: &Option<mpsc::Sender<HealAction>>, a: HealAction) {
    tracing::info!(
        "{}/{}: {:?} (attempt {}/{}, {})",
        a.project,
        a.service,
        a.kind,
        a.attempt,
        a.cap,
        a.trigger
    );
    if a.kind == HealKind::Restarted {
        tokio::spawn(crate::orchestrator::notify_heal_restart(a.clone()));
    }
    if let Some(tx) = actions
        && let Err(e) = tx.try_send(a)
    {
        tracing::warn!("health action not delivered: {e}");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn trigger(c: &ContainerInfo) -> String {
    match c.failing_streak {
        Some(n) => format!("unhealthy (failing streak {n})"),
        None => "unhealthy".into(),
    }
}

/// Decides and carries out what to do with one listed container.
async fn step(
    e: &Engine,
    listed: &ContainerInfo,
    t: &mut Track,
    policy: &Policy,
    control: &Control,
) -> Option<HealAction> {
    let now = Instant::now();
    while t
        .restarts
        .front()
        .is_some_and(|r| now.duration_since(*r) >= policy.window)
    {
        t.restarts.pop_front();
    }
    if listed.health.as_deref() == Some("healthy") {
        t.gave_up = false;
        t.reported_wait = None;
        return None;
    }
    if listed.health.as_deref() != Some("unhealthy")
        || listed.heal == HealthOnFailure::None
        || control.is_paused(&listed.project)
    {
        return None;
    }
    let done = u32::try_from(t.restarts.len()).unwrap_or(u32::MAX);
    let action = |kind, attempt, cap, next: Option<Duration>, err: Option<String>| HealAction {
        project: listed.project.clone(),
        pod: listed.pod.clone(),
        service: listed.service.clone(),
        container: listed.name.clone(),
        container_id: listed.id.clone(),
        engine: e.kind,
        kind,
        attempt,
        cap,
        next_delay_secs: next.map(|d| d.as_secs().max(1)),
        trigger: trigger(listed),
        error: err,
        at_ms: now_ms(),
    };
    if listed.heal == HealthOnFailure::Restart {
        if t.gave_up {
            return None;
        }
        if done >= policy.max_restarts {
            t.gave_up = true;
            return Some(action(
                HealKind::GaveUp,
                done,
                policy.max_restarts,
                None,
                None,
            ));
        }
        let wait = policy.delay(done);
        if let Some(last) = t.restarts.back() {
            let since = now.duration_since(*last);
            if since < wait {
                if t.reported_wait == Some(*last) {
                    return None;
                }
                t.reported_wait = Some(*last);
                return Some(action(
                    HealKind::BackingOff,
                    done + 1,
                    policy.max_restarts,
                    Some(wait - since),
                    None,
                ));
            }
        }
    }

    // A gantry command working on the project wins; try again next poll.
    let Ok(_lock) = lock(&listed.project) else {
        tracing::debug!("{}: busy, skipping this poll", listed.project);
        return None;
    };
    // Pausing waits on this guard, so a pause that lands after the check
    // above is still honored, and one that lands after this point waits for
    // the action to finish.
    let _acting = control.acting.lock().await;
    if control.is_paused(&listed.project) {
        return None;
    }
    // Confirm under the lock: it may have been stopped or recreated.
    match e.inspect(&listed.id).await {
        Ok(Some(c)) if c.status == "running" && c.health.as_deref() == Some("unhealthy") => {}
        _ => return None,
    }
    match listed.heal {
        HealthOnFailure::Restart => {
            let r = e.restart(&listed.id).await;
            t.restarts.push_back(Instant::now());
            let attempt = done + 1;
            Some(match r {
                Ok(()) => action(
                    HealKind::Restarted,
                    attempt,
                    policy.max_restarts,
                    None,
                    None,
                ),
                Err(err) => action(
                    HealKind::Failed,
                    attempt,
                    policy.max_restarts,
                    None,
                    Some(err.to_string()),
                ),
            })
        }
        HealthOnFailure::Stop => Some(match e.stop(&listed.id, None).await {
            Ok(()) => action(HealKind::Stopped, 1, 1, None, None),
            Err(err) => action(HealKind::Failed, 1, 1, None, Some(err.to_string())),
        }),
        HealthOnFailure::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap() {
        let p = Policy::default();
        assert_eq!(p.delay(0), Duration::ZERO);
        assert_eq!(p.delay(1), Duration::from_secs(30));
        assert_eq!(p.delay(2), Duration::from_secs(60));
        assert_eq!(p.delay(3), Duration::from_secs(120));
        assert_eq!(p.delay(4), Duration::from_secs(240));
        assert_eq!(p.delay(5), Duration::from_secs(300));
        assert_eq!(p.delay(40), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn control_pauses_per_project() {
        let c = Control::default();
        let d = c.clone();
        c.pause("a").await;
        assert!(d.is_paused("a"));
        assert!(!d.is_paused("b"));
        d.resume("a");
        assert!(!c.is_paused("a"));
    }

    #[tokio::test]
    async fn pause_waits_for_an_action_in_progress() {
        let c = Control::default();
        let acting = c.acting.clone().lock_owned().await;
        let d = c.clone();
        let paused = tokio::spawn(async move { d.pause("a").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(c.is_paused("a"), "marked paused at once");
        assert!(!paused.is_finished(), "returns only after the action");
        drop(acting);
        tokio::time::timeout(Duration::from_secs(1), paused)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn actions_serialize_snake_case() {
        let a = HealAction {
            project: "p".into(),
            pod: "web".into(),
            service: "app".into(),
            container: "p-app".into(),
            container_id: "abc".into(),
            engine: RuntimeKind::Docker,
            kind: HealKind::BackingOff,
            attempt: 2,
            cap: 5,
            next_delay_secs: Some(30),
            trigger: "unhealthy".into(),
            error: None,
            at_ms: 1,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["kind"], "backing_off");
        assert_eq!(v["engine"], "docker");
        assert_eq!(v["next_delay_secs"], 30);
    }
}
