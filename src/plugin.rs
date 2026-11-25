//! Plugins: separately installed programs that gantry runs on lifecycle
//! hooks (docs/spec/plugins.md).
//!
//! A plugin is a directory holding `plugin.toml`. Gantry reads only the
//! manifests up front, and starts a plugin the first time a command reaches
//! a hook the plugin subscribed to. Messages are JSON lines over the
//! plugin's stdin and stdout. Hooks fail open: a plugin that is missing,
//! crashes, times out or reports an error is a warning and never fails the
//! command.

use crate::compose::units::parse_duration;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

/// The protocol version this gantry speaks.
pub const API: u32 = 1;

const MANIFEST: &str = "plugin.toml";
const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ASYNC_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_ADDRESS_TIMEOUT: Duration = Duration::from_secs(15);

/// A point in a command where plugins are told what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    PreUp,
    PostUp,
    PreDown,
    PostDown,
    PostStart,
    PostStop,
    PostRestart,
    HealRestart,
    Boot,
}

impl Hook {
    pub const ALL: [Hook; 9] = [
        Hook::PreUp,
        Hook::PostUp,
        Hook::PreDown,
        Hook::PostDown,
        Hook::PostStart,
        Hook::PostStop,
        Hook::PostRestart,
        Hook::HealRestart,
        Hook::Boot,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Hook::PreUp => "pre_up",
            Hook::PostUp => "post_up",
            Hook::PreDown => "pre_down",
            Hook::PostDown => "post_down",
            Hook::PostStart => "post_start",
            Hook::PostStop => "post_stop",
            Hook::PostRestart => "post_restart",
            Hook::HealRestart => "heal_restart",
            Hook::Boot => "boot",
        }
    }

    fn parse(s: &str) -> Option<Hook> {
        Hook::ALL.into_iter().find(|h| h.as_str() == s)
    }
}

/// Whether gantry waits for the plugin to handle an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Sync,
    Async,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookSpec {
    pub mode: Mode,
    pub timeout: Duration,
}

/// The `[address]` table: this plugin allocates `address: { from: ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AddressSpec {
    pub from: String,
    pub timeout: Duration,
}

/// A plugin as its manifest describes it, after host overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: Option<String>,
    /// The directory holding `plugin.toml`; relative `exec` paths resolve
    /// against it and the plugin starts in it.
    pub dir: PathBuf,
    pub exec: Vec<String>,
    pub hooks: IndexMap<Hook, HookSpec>,
    pub address: Option<AddressSpec>,
    /// False when `/etc/gantry/plugins.toml` disables it on this host.
    pub host_enabled: bool,
}

#[derive(Deserialize)]
struct RawManifest {
    api: u32,
    name: String,
    version: Option<String>,
    exec: Vec<String>,
    #[serde(default)]
    hooks: IndexMap<String, RawHook>,
    address: Option<RawAddress>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHook {
    mode: Mode,
    timeout: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAddress {
    from: String,
    timeout: Option<String>,
}

/// One plugin's entry in `/etc/gantry/plugins.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEntry {
    enabled: Option<bool>,
    #[serde(default)]
    hooks: IndexMap<String, Mode>,
}

/// Where plugins and their files live. [`Paths::system`] is the installed
/// layout; tests point it at a temporary directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Searched in order; the first manifest with a name wins.
    pub dirs: Vec<PathBuf>,
    /// Gantry's per host overrides.
    pub host_config: PathBuf,
    /// Plugin settings files, `<name>.toml`, read by the plugins.
    pub config_dir: PathBuf,
    /// Lock files handed to plugins, one per plugin and project.
    pub lock_dir: PathBuf,
    /// Plugin stderr, appended to `<name>.log`.
    pub log_dir: PathBuf,
}

impl Paths {
    /// The installed layout. Debug builds search `GANTRY_PLUGIN_DIR`
    /// instead when it is set, for tests; release builds never do.
    pub fn system() -> Self {
        let dirs = match std::env::var_os("GANTRY_PLUGIN_DIR") {
            Some(d) if cfg!(debug_assertions) => vec![PathBuf::from(d)],
            _ => vec![
                PathBuf::from("/usr/local/lib/gantry/plugins"),
                PathBuf::from("/usr/lib/gantry/plugins"),
            ],
        };
        Self {
            dirs,
            host_config: PathBuf::from("/etc/gantry/plugins.toml"),
            config_dir: PathBuf::from("/etc/gantry/plugins"),
            lock_dir: crate::pod::state::run_dir().join("plugins"),
            log_dir: PathBuf::from("/var/log/gantry/plugins"),
        }
    }
}

/// The plugins installed on this host.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub plugins: Vec<Manifest>,
    pub warnings: Vec<String>,
    paths: Option<Paths>,
}

impl Registry {
    /// Reads every manifest under the system plugin directories. A host
    /// without plugins costs two failed directory reads.
    pub fn system() -> Self {
        let r = Self::load(Paths::system());
        for w in &r.warnings {
            tracing::warn!("{w}");
        }
        r
    }

    pub fn load(paths: Paths) -> Self {
        let mut warnings = Vec::new();
        let host = read_host_config(&paths.host_config, &mut warnings);
        let mut plugins: Vec<Manifest> = Vec::new();
        for dir in &paths.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path().join(MANIFEST)))
                .filter(|p| p.is_file())
                .collect();
            found.sort();
            for path in found {
                match read_manifest(&path) {
                    Ok(mut m) => {
                        if plugins.iter().any(|p| p.name == m.name) {
                            continue;
                        }
                        if let Some(h) = host.get(&m.name) {
                            m.host_enabled = h.enabled.unwrap_or(true);
                            for (hook, mode) in &h.hooks {
                                match Hook::parse(hook).and_then(|k| m.hooks.get_mut(&k)) {
                                    Some(spec) => spec.mode = *mode,
                                    None => warnings.push(format!(
                                        "{}: {}: {hook:?} is not a hook the plugin uses",
                                        paths.host_config.display(),
                                        m.name
                                    )),
                                }
                            }
                        }
                        plugins.push(m);
                    }
                    Err(e) => warnings.push(format!("plugin {}: {e}; skipped", path.display())),
                }
            }
        }
        Self {
            plugins,
            warnings,
            paths: Some(paths),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Manifest> {
        self.plugins.iter().find(|p| p.name == name)
    }

    /// The plugin that allocates `address: { from: <from> }`.
    pub fn address_provider(&self, from: &str) -> Option<&Manifest> {
        self.plugins
            .iter()
            .find(|p| p.address.as_ref().is_some_and(|a| a.from == from))
    }
}

fn read_host_config(path: &Path, warnings: &mut Vec<String>) -> IndexMap<String, HostEntry> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            warnings.push(format!("{}: {e}; ignored", path.display()));
            IndexMap::new()
        }),
        Err(_) => IndexMap::new(),
    }
}

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    check_owner(path)?;
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw: RawManifest = toml::from_str(&text).map_err(|e| e.to_string())?;
    if raw.api != API {
        return Err(format!("api {} (this gantry speaks {API})", raw.api));
    }
    if !valid_name(&raw.name) {
        return Err(format!(
            "name {:?}: use lowercase letters, digits, - and _",
            raw.name
        ));
    }
    if raw.exec.is_empty() || raw.exec[0].is_empty() {
        return Err("exec is empty".into());
    }
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let timeout = |t: &Option<String>, default: Duration| match t {
        Some(t) => parse_duration(t).map_err(|e| format!("timeout {t:?}: {e}")),
        None => Ok(default),
    };
    let mut hooks = IndexMap::new();
    for (name, h) in raw.hooks {
        let hook = Hook::parse(&name).ok_or_else(|| format!("unknown hook {name:?}"))?;
        let default = match h.mode {
            Mode::Sync => DEFAULT_SYNC_TIMEOUT,
            Mode::Async => DEFAULT_ASYNC_TIMEOUT,
        };
        hooks.insert(
            hook,
            HookSpec {
                mode: h.mode,
                timeout: timeout(&h.timeout, default)?,
            },
        );
    }
    let address = raw
        .address
        .map(|a| -> Result<AddressSpec, String> {
            Ok(AddressSpec {
                timeout: timeout(&a.timeout, DEFAULT_ADDRESS_TIMEOUT)?,
                from: a.from,
            })
        })
        .transpose()?;
    Ok(Manifest {
        name: raw.name,
        version: raw.version,
        dir,
        exec: raw.exec,
        hooks,
        address,
        host_enabled: true,
    })
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Plugins run with gantry's privileges, so their manifest and program
/// must be owned by root (or by the user gantry runs as) and not writable
/// by anyone else.
fn check_owner(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let me = nix::unistd::geteuid().as_raw();
    if md.uid() != 0 && md.uid() != me {
        return Err(format!("{} is not owned by root", path.display()));
    }
    if md.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by its group or others",
            path.display()
        ));
    }
    Ok(())
}

/// A project's `plugins.<name>` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectPlugin {
    pub enabled: bool,
    /// Every key besides `enabled`, passed to the plugin as `settings`.
    pub settings: serde_json::Map<String, Value>,
}

/// A plugin's reply to one message.
type Reply = Result<Value, String>;

/// The plugins of one command for one project, started on first use.
pub struct Plugins {
    registry: Arc<Registry>,
    project: String,
    settings: IndexMap<String, ProjectPlugin>,
    running: tokio::sync::Mutex<HashMap<String, Arc<Running>>>,
}

struct Running {
    tx: Mutex<Option<mpsc::Sender<Outgoing>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
    /// Cleared by the reader once the plugin's stdout closes.
    alive: Arc<std::sync::atomic::AtomicBool>,
    next_id: std::sync::atomic::AtomicU64,
}

struct Outgoing {
    line: Vec<u8>,
    /// Told once the line is in the pipe, or why it could not be written.
    written: oneshot::Sender<Result<(), String>>,
}

impl Plugins {
    pub fn new(
        registry: Arc<Registry>,
        project: &str,
        settings: IndexMap<String, ProjectPlugin>,
    ) -> Self {
        Self {
            registry,
            project: project.to_string(),
            settings,
            running: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Plugins enabled for this project, on this host.
    fn enabled(&self) -> impl Iterator<Item = &Manifest> {
        self.registry
            .plugins
            .iter()
            .filter(|m| m.host_enabled && self.settings.get(&m.name).is_none_or(|s| s.enabled))
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled().any(|m| m.name == name)
    }

    /// True when some enabled plugin subscribed to `hook`, so callers can
    /// skip building event data nobody reads.
    pub fn wants(&self, hook: Hook) -> bool {
        self.enabled().any(|m| m.hooks.contains_key(&hook))
    }

    /// Tells every enabled plugin subscribed to `hook`. Never fails: each
    /// problem is logged as a warning.
    pub async fn emit(&self, hook: Hook, data: &Value) {
        let targets: Vec<(Manifest, HookSpec)> = self
            .enabled()
            .filter_map(|m| m.hooks.get(&hook).map(|s| (m.clone(), s.clone())))
            .collect();
        let calls = targets.iter().map(|(m, spec)| async move {
            if let Err(e) = self.event(m, hook, spec, data).await {
                tracing::warn!("plugin {}: {}: {e}", m.name, hook.as_str());
            }
        });
        futures_util::future::join_all(calls).await;
    }

    async fn event(
        &self,
        m: &Manifest,
        hook: Hook,
        spec: &HookSpec,
        data: &Value,
    ) -> Result<(), String> {
        let msg = json!({
            "type": "event",
            "hook": hook.as_str(),
            "mode": spec.mode,
            "deadline_ms": deadline_ms(spec.timeout),
            "data": self.with_settings(&m.name, data),
        });
        let run = self.start(m).await?;
        match spec.mode {
            Mode::Sync => {
                tokio::time::timeout(spec.timeout, run.request(msg))
                    .await
                    .map_err(|_| format!("no reply within {:?}", spec.timeout))??;
            }
            Mode::Async => {
                tokio::time::timeout(spec.timeout, run.post(msg))
                    .await
                    .map_err(|_| format!("not accepted within {:?}", spec.timeout))??;
            }
        }
        Ok(())
    }

    /// Asks the plugin that provides `from` for a value: the one call that
    /// fails the command when it fails.
    pub async fn allocate(&self, from: &str, data: &Value) -> crate::Result<Value> {
        let Some(m) = self.registry.address_provider(from) else {
            return Err(crate::Error::config(format!(
                "address from {from:?}: no installed plugin provides it"
            )));
        };
        if !self.is_enabled(&m.name) {
            return Err(crate::Error::config(format!(
                "address from {from:?}: plugin {} is disabled",
                m.name
            )));
        }
        let timeout = m
            .address
            .as_ref()
            .map_or(DEFAULT_ADDRESS_TIMEOUT, |a| a.timeout);
        let msg = json!({
            "type": "allocate",
            "deadline_ms": deadline_ms(timeout),
            "data": self.with_settings(&m.name, data),
        });
        let fail = |e: String| crate::Error::State(format!("plugin {}: allocate: {e}", m.name));
        let run = self.start(m).await.map_err(fail)?;
        tokio::time::timeout(timeout, run.request(msg))
            .await
            .map_err(|_| fail(format!("no reply within {timeout:?}")))?
            .map_err(fail)
    }

    fn with_settings(&self, name: &str, data: &Value) -> Value {
        let mut data = data.clone();
        if let Value::Object(o) = &mut data {
            let settings = self
                .settings
                .get(name)
                .map(|s| Value::Object(s.settings.clone()))
                .unwrap_or_else(|| json!({}));
            o.insert("settings".into(), settings);
        }
        data
    }

    async fn start(&self, m: &Manifest) -> Result<Arc<Running>, String> {
        let mut running = self.running.lock().await;
        if let Some(r) = running.get(&m.name) {
            return Ok(r.clone());
        }
        let paths = self.registry.paths.clone().unwrap_or_else(Paths::system);
        let r = Arc::new(spawn(m, &paths, &self.project)?);
        running.insert(m.name.clone(), r.clone());
        Ok(r)
    }
}

impl Running {
    async fn request(&self, mut msg: Value) -> Reply {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        msg["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("plugin exited".into());
            }
            pending.insert(id, tx);
        }
        if let Err(e) = self.post(msg).await {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        rx.await.unwrap_or_else(|_| Err("plugin exited".into()))
    }

    /// Writes one message and returns once it is in the pipe.
    async fn post(&self, msg: Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let (written, done) = oneshot::channel();
        let sent = lock(&self.tx)
            .as_ref()
            .map(|tx| tx.send(Outgoing { line, written }).is_ok())
            .unwrap_or(false);
        if !sent {
            return Err("plugin exited".into());
        }
        done.await.unwrap_or_else(|_| Err("plugin exited".into()))
    }
}

impl Drop for Plugins {
    /// Closes every plugin's stdin, so each finishes what it was sent and
    /// exits. Async work carries on after gantry is gone.
    fn drop(&mut self) {
        if let Ok(running) = self.running.try_lock() {
            for r in running.values() {
                lock(&r.tx).take();
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn deadline_ms(after: Duration) -> u64 {
    (SystemTime::now() + after)
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Starts a plugin in its own session, so it can finish async events after
/// gantry exits, and sends it `hello`.
fn spawn(m: &Manifest, paths: &Paths, project: &str) -> Result<Running, String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = m.dir.join(&m.exec[0]);
    check_owner(&exe)?;
    let lock_dir = paths.lock_dir.join(&m.name);
    crate::pod::state::ensure_dir(&lock_dir).map_err(|e| e.to_string())?;
    let lock_file = lock_dir.join(format!("{project}.lock"));

    let mut cmd = Command::new(&exe);
    cmd.args(&m.exec[1..])
        .current_dir(&m.dir)
        .env_clear()
        .env("GANTRY_PLUGIN_NAME", &m.name)
        .env(
            "GANTRY_PLUGIN_CONFIG",
            paths.config_dir.join(format!("{}.toml", m.name)),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_file(paths, &m.name));
    for var in ["PATH", "LANG", "LC_ALL"] {
        if let Some(v) = std::env::var_os(var) {
            cmd.env(var, v);
        }
    }
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start {}: {e}", exe.display()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("no pipes".into());
    };

    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>> = Arc::default();
    let (tx, rx) = mpsc::channel::<Outgoing>();
    let name = m.name.clone();

    // Writer: SIGPIPE is blocked on this thread only, so a plugin that has
    // exited turns into an EPIPE error instead of killing gantry.
    std::thread::spawn(move || {
        let mut set = nix::sys::signal::SigSet::empty();
        set.add(nix::sys::signal::Signal::SIGPIPE);
        let _ = set.thread_block();
        let mut stdin = stdin;
        let mut broken: Option<String> = None;
        for out in rx {
            let res = match &broken {
                Some(e) => Err(e.clone()),
                None => stdin
                    .write_all(&out.line)
                    .and_then(|()| stdin.flush())
                    .map_err(|e| e.to_string()),
            };
            if let Err(e) = &res {
                broken = Some(e.clone());
            }
            let _ = out.written.send(res);
        }
    });

    // Reader: routes each reply to the request waiting on its id.
    let waiting = pending.clone();
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reading = alive.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                tracing::warn!("plugin {name}: not a JSON line: {line:.200}");
                continue;
            };
            let Some(id) = v.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let reply = if v.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed without a message")
                    .to_string())
            };
            if let Some(tx) = lock(&waiting).remove(&id) {
                let _ = tx.send(reply);
            }
        }
        let mut waiting = lock(&waiting);
        reading.store(false, std::sync::atomic::Ordering::SeqCst);
        for (_, tx) in waiting.drain() {
            let _ = tx.send(Err("plugin exited".into()));
        }
    });

    // Reaper, so a long running gantry (supervise) leaves no zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let run = Running {
        tx: Mutex::new(Some(tx)),
        pending,
        alive,
        next_id: std::sync::atomic::AtomicU64::new(1),
    };
    let hello = json!({
        "type": "hello",
        "api": API,
        "gantry": env!("CARGO_PKG_VERSION"),
        "plugin": m.name,
        "host": nix::unistd::gethostname().ok().and_then(|h| h.into_string().ok()),
        "project": project,
        "lock": lock_file,
    });
    let mut line = serde_json::to_vec(&hello).map_err(|e| e.to_string())?;
    line.push(b'\n');
    let (written, _) = oneshot::channel();
    if let Some(tx) = lock(&run.tx).as_ref() {
        let _ = tx.send(Outgoing { line, written });
    }
    Ok(run)
}

/// The plugin's stderr: appended to its log file, or discarded when the
/// log directory cannot be written.
fn log_file(paths: &Paths, name: &str) -> std::process::Stdio {
    use std::os::unix::fs::OpenOptionsExt;
    let open = || -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(&paths.log_dir)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(paths.log_dir.join(format!("{name}.log")))
    };
    match open() {
        Ok(f) => f.into(),
        Err(e) => {
            tracing::debug!("plugin {name}: log {}: {e}", paths.log_dir.display());
            std::process::Stdio::null()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let paths = Paths {
                dirs: vec![root.join("local"), root.join("lib")],
                host_config: root.join("plugins.toml"),
                config_dir: root.join("etc"),
                lock_dir: root.join("run"),
                log_dir: root.join("log"),
            };
            Self { _tmp: tmp, paths }
        }

        fn root(&self) -> &Path {
            self.paths.host_config.parent().unwrap()
        }

        /// Installs a plugin whose program is a shell script.
        fn plugin(&self, dir: usize, name: &str, manifest: &str, script: &str) -> PathBuf {
            let d = self.paths.dirs[dir].join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(MANIFEST), manifest).unwrap();
            std::fs::set_permissions(d.join(MANIFEST), std::fs::Permissions::from_mode(0o644))
                .unwrap();
            let prog = d.join("run");
            std::fs::write(&prog, format!("#!/bin/sh\n{script}")).unwrap();
            std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
            d
        }

        fn registry(&self) -> Arc<Registry> {
            Arc::new(Registry::load(self.paths.clone()))
        }
    }

    /// Records every line it receives and answers each one that has an id.
    const ECHO: &str = r#"out="$(dirname "$0")/got"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$out"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  [ -n "$id" ] && printf '{"id":%s,"ok":true,"result":{"address":"192.0.2.218/29"}}\n' "$id"
done
"#;

    fn manifest(name: &str, hooks: &str) -> String {
        format!("api = 1\nname = \"{name}\"\nexec = [\"run\"]\n{hooks}")
    }

    fn got(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("got"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn manifests_and_overrides() {
        let f = Fixture::new();
        f.plugin(
            0,
            "nb",
            &manifest(
                "nautobot",
                "[hooks]\npost_up = { mode = \"sync\", timeout = \"5s\" }\npost_down = { mode = \"async\" }\n[address]\nfrom = \"nautobot\"\n",
            ),
            "",
        );
        // Same name further down the search path loses.
        f.plugin(1, "nb2", &manifest("nautobot", ""), "");
        f.plugin(
            1,
            "bad",
            &manifest("bad", "[hooks]\nsideways = { mode = \"sync\" }\n"),
            "",
        );
        f.plugin(
            1,
            "future",
            "api = 2\nname = \"future\"\nexec = [\"run\"]\n",
            "",
        );
        std::fs::write(
            &f.paths.host_config,
            "[nautobot.hooks]\npost_up = \"async\"\n",
        )
        .unwrap();

        let r = f.registry();
        assert_eq!(r.plugins.len(), 1, "{:?}", r.warnings);
        let m = &r.plugins[0];
        assert_eq!(m.dir, f.paths.dirs[0].join("nb"));
        assert_eq!(m.hooks[&Hook::PostUp].mode, Mode::Async);
        assert_eq!(m.hooks[&Hook::PostUp].timeout, Duration::from_secs(5));
        assert_eq!(m.hooks[&Hook::PostDown].timeout, DEFAULT_ASYNC_TIMEOUT);
        assert!(r.address_provider("nautobot").is_some());
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    }

    #[test]
    fn writable_manifest_is_refused() {
        let f = Fixture::new();
        let d = f.plugin(0, "p", &manifest("p", ""), "");
        std::fs::set_permissions(d.join(MANIFEST), std::fs::Permissions::from_mode(0o666)).unwrap();
        let r = f.registry();
        assert!(r.plugins.is_empty());
        assert!(r.warnings[0].contains("writable"), "{:?}", r.warnings);
    }

    #[tokio::test]
    async fn sync_and_async_events_reach_the_plugin() {
        let f = Fixture::new();
        let d = f.plugin(
            0,
            "rec",
            &manifest(
                "rec",
                "[hooks]\npost_up = { mode = \"sync\" }\npost_down = { mode = \"async\" }\n[address]\nfrom = \"rec\"\n",
            ),
            ECHO,
        );
        let mut settings = IndexMap::new();
        let mut s = serde_json::Map::new();
        s.insert("tenant".into(), json!("acme"));
        settings.insert(
            "rec".into(),
            ProjectPlugin {
                enabled: true,
                settings: s,
            },
        );
        let p = Plugins::new(f.registry(), "shop", settings);
        assert!(p.wants(Hook::PostUp) && !p.wants(Hook::PreUp));
        p.emit(Hook::PreUp, &json!({"project": "shop"})).await;
        p.emit(Hook::PostUp, &json!({"project": "shop"})).await;
        let a = p.allocate("rec", &json!({"pod": "web"})).await.unwrap();
        assert_eq!(a["address"], "192.0.2.218/29");
        p.emit(Hook::PostDown, &json!({"project": "shop"})).await;
        drop(p);
        // The async event needs no reply; wait for the plugin to record it.
        for _ in 0..50 {
            if got(&d).len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let msgs = got(&d);
        let kinds: Vec<_> = msgs
            .iter()
            .map(|m| m["hook"].as_str().unwrap_or(m["type"].as_str().unwrap()))
            .collect();
        assert_eq!(kinds, ["hello", "post_up", "allocate", "post_down"]);
        assert_eq!(msgs[0]["project"], "shop");
        assert_eq!(
            msgs[0]["lock"],
            json!(f.paths.lock_dir.join("rec/shop.lock"))
        );
        assert_eq!(msgs[1]["data"]["settings"]["tenant"], "acme");
        assert_eq!(msgs[3]["mode"], "async");
        assert!(msgs[3].get("id").is_none());
    }

    #[tokio::test]
    async fn failures_are_warnings_but_allocation_fails() {
        let f = Fixture::new();
        f.plugin(
            0,
            "dies",
            &manifest(
                "dies",
                "[hooks]\npost_up = { mode = \"sync\", timeout = \"2s\" }\n[address]\nfrom = \"dies\"\n",
            ),
            "exit 3\n",
        );
        f.plugin(
            0,
            "slow",
            &manifest(
                "slow",
                "[hooks]\npost_up = { mode = \"sync\", timeout = \"200ms\" }\n",
            ),
            "sleep 5\n",
        );
        f.plugin(
            0,
            "says-no",
            &manifest("says-no", "[address]\nfrom = \"no\"\n"),
            "read -r hello; read -r line; echo '{\"id\":1,\"ok\":false,\"error\":\"pool empty\"}'\n",
        );
        let p = Plugins::new(f.registry(), "shop", IndexMap::new());
        let started = std::time::Instant::now();
        p.emit(Hook::PostUp, &json!({})).await;
        assert!(started.elapsed() < Duration::from_secs(3));
        let e = p.allocate("dies", &json!({})).await.unwrap_err();
        assert!(e.to_string().contains("plugin dies"), "{e}");
        let e = p.allocate("no", &json!({})).await.unwrap_err();
        assert!(e.to_string().contains("pool empty"), "{e}");
        let e = p.allocate("nobody", &json!({})).await.unwrap_err();
        assert!(e.to_string().contains("no installed plugin"), "{e}");
    }

    #[tokio::test]
    async fn disabled_plugins_are_never_started() {
        let f = Fixture::new();
        let d = f.plugin(
            0,
            "off",
            &manifest("off", "[hooks]\npost_up = { mode = \"sync\" }\n"),
            ECHO,
        );
        let mut settings = IndexMap::new();
        settings.insert(
            "off".into(),
            ProjectPlugin {
                enabled: false,
                settings: Default::default(),
            },
        );
        let p = Plugins::new(f.registry(), "shop", settings);
        assert!(!p.wants(Hook::PostUp));
        p.emit(Hook::PostUp, &json!({})).await;
        drop(p);

        std::fs::write(f.root().join("plugins.toml"), "[off]\nenabled = false\n").unwrap();
        let p = Plugins::new(f.registry(), "shop", IndexMap::new());
        assert!(!p.wants(Hook::PostUp));
        p.emit(Hook::PostUp, &json!({})).await;
        drop(p);
        assert!(got(&d).is_empty());
    }
}
