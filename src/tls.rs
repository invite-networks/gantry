//! Pod certificates (docs/spec/certificates.md).
//!
//! An issuer plugin makes the key and the certificate. Gantry keeps them
//! under the pod's directory in /run, writes each service's files in the
//! layout it asked for, mounts that directory into the service's
//! container, refreshes the certificate before it runs out, and tells each
//! service in the way it asked. Problems are reported loudly: errors in
//! the log, warnings on commands, and a `tls_problem` plugin event.

use crate::engine::exec::ExecRequest;
use crate::errors::{Context, Error, Result};
use crate::orchestrator::Orchestrator;
use crate::plugin::Hook;
use crate::pod::state;
use crate::project::{OnRenew, Pod, PodTls, Project, Service, TlsContent, TlsFile, TlsFormat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A certificate within this long of its end is a problem to report.
pub const WARN_BEFORE: Duration = Duration::from_secs(4 * 3600);

/// A pod's key and certificate, as the issuer returned them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issued {
    pub names: Vec<String>,
    pub key_type: String,
    /// PKCS#8 PEM.
    pub key: String,
    /// The certificate alone, PEM.
    pub cert: String,
    /// The intermediates, PEM.
    pub chain: String,
    pub root: String,
    pub issued_ms: u64,
    pub not_after_ms: u64,
}

impl Issued {
    pub fn expired(&self) -> bool {
        now_ms() >= self.not_after_ms
    }

    /// Past two thirds of its lifetime.
    pub fn due(&self) -> bool {
        let life = self.not_after_ms.saturating_sub(self.issued_ms);
        now_ms() >= self.issued_ms + life * 2 / 3
    }

    /// What the files say, for `ps`.
    pub fn describe(&self) -> String {
        let left = self.not_after_ms.saturating_sub(now_ms()) / 1000;
        if self.expired() {
            format!("{} EXPIRED", self.names.join(", "))
        } else {
            format!(
                "{} expires in {}h{:02}m",
                self.names.join(", "),
                left / 3600,
                left % 3600 / 60
            )
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pod_tls_dir(project: &str, pod: &str) -> PathBuf {
    state::pod_dir(project, pod).join("tls")
}

/// The directory a service's files are written to and mounted from.
pub fn service_dir(project: &str, pod: &str, service: &str) -> PathBuf {
    pod_tls_dir(project, pod).join("svc").join(service)
}

fn issued_path(project: &str, pod: &str) -> PathBuf {
    pod_tls_dir(project, pod).join("issued.json")
}

/// The pod's current certificate, if it has one.
pub fn load(project: &str, pod: &str) -> Option<Issued> {
    let b = std::fs::read(issued_path(project, pod)).ok()?;
    serde_json::from_slice(&b).ok()
}

fn save(project: &str, pod: &str, i: &Issued) -> Result<()> {
    let dir = pod_tls_dir(project, pod);
    make_dir(&dir, 0o700)?;
    let data = serde_json::to_vec(i).map_err(|e| Error::State(e.to_string()))?;
    write_atomic(&issued_path(project, pod), &data, 0o600, None)
}

fn make_dir(dir: &Path, mode: u32) -> Result<()> {
    std::fs::create_dir_all(dir).ctx(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
        .ctx(|| format!("chmod {}", dir.display()))
}

/// Writes beside the target and renames over it, so a reader (a
/// container, through its mounted directory) never sees half a file.
fn write_atomic(path: &Path, data: &[u8], mode: u32, owner: Option<(u32, u32)>) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, data).ctx(|| format!("write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .ctx(|| format!("chmod {}", tmp.display()))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid))
            .ctx(|| format!("chown {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).ctx(|| format!("write {}", path.display()))
}

/// Reads an `issue` or `renew` answer.
fn from_reply(v: &Value, tls: &PodTls) -> Result<Issued> {
    let s = |k: &str| -> Result<String> {
        v.get(k)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| Error::State(format!("certificate issuer: the answer has no {k}")))
    };
    let key = s("key")?;
    let cert = s("cert")?;
    if !key.contains("PRIVATE KEY-----") || !cert.contains("-----BEGIN CERTIFICATE-----") {
        return Err(Error::State(
            "certificate issuer: the key or certificate is not PEM".into(),
        ));
    }
    let not_after_ms = v
        .get("not_after")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::State("certificate issuer: the answer has no not_after".into()))?;
    Ok(Issued {
        names: tls.names.clone(),
        key_type: tls.key_type.as_str().into(),
        key,
        cert,
        chain: v.get("chain").and_then(Value::as_str).unwrap_or("").into(),
        root: s("root")?,
        issued_ms: now_ms(),
        not_after_ms,
    })
}

async fn issue(o: &Orchestrator, pod: &Pod, tls: &PodTls) -> Result<Issued> {
    let data = json!({
        "project": o.project.name,
        "pod": pod.name,
        "names": tls.names,
        "key_type": tls.key_type.as_str(),
    });
    let v = o
        .plugins
        .certificate(tls.issuer.as_deref(), "issue", &data)
        .await?;
    from_reply(&v, tls)
}

async fn renew(o: &Orchestrator, pod: &Pod, tls: &PodTls, cur: &Issued) -> Result<Issued> {
    let data = json!({
        "project": o.project.name,
        "pod": pod.name,
        "cert": format!("{}{}", cur.cert, cur.chain),
        "key": cur.key,
    });
    let v = o
        .plugins
        .certificate(tls.issuer.as_deref(), "renew", &data)
        .await?;
    let mut i = from_reply(&v, tls)?;
    i.names.clone_from(&cur.names);
    Ok(i)
}

/// Gives the pod a certificate if it has none, or one that is expired or
/// for other names or another key type, and writes every service's files.
/// Called by `up` before containers start. Fails when the issuer does,
/// unless the pod's tls is optional.
pub async fn ensure(o: &Orchestrator, pod: &Pod) -> Result<()> {
    let Some(tls) = &pod.tls else {
        return Ok(());
    };
    let p = &o.project.name;
    let current = load(p, &pod.name);
    let fits =
        |i: &Issued| !i.expired() && i.names == tls.names && i.key_type == tls.key_type.as_str();
    let issued = match current {
        Some(i) if fits(&i) => i,
        _ => match issue(o, pod, tls).await {
            Ok(i) => {
                save(p, &pod.name, &i)?;
                println!("Pod {} certificate: {}", pod.name, i.describe());
                i
            }
            Err(e) if tls.optional => {
                let msg = format!("pod {}: certificate not issued: {e}", pod.name);
                report(o, Some(&pod.name), &msg).await;
                // Mount points must exist for the containers to start.
                for svc in o.project.services_in_pod(&pod.name) {
                    make_dir(&service_dir(p, &pod.name, &svc.name), 0o755)?;
                }
                return Ok(());
            }
            Err(e) => {
                return Err(Error::State(format!("pod {}: certificate: {e}", pod.name)));
            }
        },
    };
    render(&o.project, pod, &issued)
}

/// Writes every tls service's files from the pod's certificate.
pub fn render(project: &Project, pod: &Pod, i: &Issued) -> Result<()> {
    for svc in project.services_in_pod(&pod.name) {
        let Some(t) = &svc.tls else { continue };
        let dir = service_dir(&project.name, &pod.name, &svc.name);
        make_dir(&dir, 0o755)?;
        for (name, f) in &t.files {
            let bytes = render_file(i, f, &dir)
                .map_err(|e| Error::State(format!("service {}: tls file {name}: {e}", svc.name)))?;
            let mode = if f.content.is_secret() || f.format == TlsFormat::Pkcs12 {
                0o400
            } else {
                0o444
            };
            write_atomic(&dir.join(name), &bytes, mode, Some(t.owner))?;
        }
        // Files the layout no longer names go.
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if !t.files.contains_key(&n) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    Ok(())
}

fn render_file(i: &Issued, f: &TlsFile, scratch: &Path) -> std::result::Result<Vec<u8>, String> {
    let fullchain = format!("{}{}", i.cert, i.chain);
    let pem = match f.content {
        TlsContent::Cert => i.cert.clone(),
        TlsContent::Chain => i.chain.clone(),
        TlsContent::Fullchain => fullchain.clone(),
        TlsContent::Key => i.key.clone(),
        TlsContent::KeyPkcs1 => openssl(&["pkey", "-traditional"], i.key.as_bytes(), &[], scratch)
            .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))?,
        TlsContent::Root => i.root.clone(),
        TlsContent::Bundle => format!("{}{fullchain}", i.key),
    };
    match f.format {
        TlsFormat::Pem => Ok(pem.into_bytes()),
        TlsFormat::Der => match f.content {
            TlsContent::Key => openssl(&["pkey", "-outform", "DER"], pem.as_bytes(), &[], scratch),
            TlsContent::KeyPkcs1 => openssl(
                &["pkey", "-traditional", "-outform", "DER"],
                i.key.as_bytes(),
                &[],
                scratch,
            ),
            _ => openssl(&["x509", "-outform", "DER"], pem.as_bytes(), &[], scratch),
        },
        TlsFormat::Pkcs12 => {
            let password = f.password.clone().unwrap_or_default();
            openssl(
                &["pkcs12", "-export", "-passout", "env:GANTRY_P12_PASS"],
                format!("{}{fullchain}", i.key).as_bytes(),
                &[("GANTRY_P12_PASS", &password)],
                scratch,
            )
        }
    }
}

/// Runs `openssl` with `input` on stdin and returns its stdout. The
/// password, when there is one, goes through the environment, not the
/// command line.
fn openssl(
    args: &[&str],
    input: &[u8],
    env: &[(&str, &str)],
    cwd: &Path,
) -> std::result::Result<Vec<u8>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("openssl");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("openssl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .map_err(|e| format!("openssl: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("openssl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "openssl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Reports a certificate problem loudly: an error in the log and on
/// stderr, and a `tls_problem` event for plugins.
pub async fn report(o: &Orchestrator, pod: Option<&str>, message: &str) {
    tracing::error!("{}: {message}", o.project.name);
    eprintln!("gantry: {}: {message}", o.project.name);
    o.notify(
        Hook::TlsProblem,
        Some(json!({ "tls_problem": { "pod": pod, "message": message } })),
    )
    .await;
}

/// One certificate watcher per host holds this.
pub fn lock_path() -> PathBuf {
    state::run_dir().join("tls.lock")
}

/// Held by the running certificate watcher.
pub struct HostLock(#[allow(dead_code)] nix::fcntl::Flock<std::fs::File>);

/// Takes the host's certificate watcher lock, or fails when another
/// watcher (`gantry supervise`, or gantry-agent) holds it.
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
            "another certificate watcher is running on this host ({} is locked)",
            path.display()
        ))),
        Err((_, e)) => Err(e).ctx(|| format!("lock {}", path.display())),
    }
}

/// The file the credential hook touches: `gantry internal tls-refresh`.
pub fn trigger_path() -> PathBuf {
    state::run_dir().join("tls-refresh")
}

/// Marks that the issuer's credential changed, so the watcher mints every
/// pod certificate again.
pub fn trigger() -> Result<()> {
    let p = trigger_path();
    if let Some(d) = p.parent() {
        state::ensure_dir(d)?;
    }
    std::fs::write(&p, now_ms().to_string()).ctx(|| format!("write {}", p.display()))
}

fn triggered_ms() -> u64 {
    std::fs::read_to_string(trigger_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Refreshes one pod's certificate when it is due, the credential changed
/// since it was issued, or its names or key type changed: mints a new one,
/// or renews the current one when minting fails. Then writes the files and
/// tells each service. Returns the problems to report.
pub async fn refresh_pod(o: &Orchestrator, pod: &Pod) -> Vec<String> {
    let Some(tls) = &pod.tls else { return vec![] };
    let p = &o.project.name;
    let mut problems = Vec::new();
    let current = load(p, &pod.name);
    let need = match &current {
        None => true,
        Some(i) => {
            i.due()
                || i.issued_ms < triggered_ms()
                || i.names != tls.names
                || i.key_type != tls.key_type.as_str()
        }
    };
    if need {
        let fresh = match issue(o, pod, tls).await {
            Ok(i) => Ok(i),
            Err(e) => {
                problems.push(format!("pod {}: mint failed: {e}", pod.name));
                match &current {
                    Some(c) if !c.expired() => renew(o, pod, tls, c).await,
                    _ => Err(e),
                }
            }
        };
        match fresh {
            Ok(i) => {
                if let Err(e) = save(p, &pod.name, &i).and_then(|()| render(&o.project, pod, &i)) {
                    problems.push(format!("pod {}: {e}", pod.name));
                } else {
                    tracing::info!("{p}/{}: certificate refreshed: {}", pod.name, i.describe());
                    for svc in o.project.services_in_pod(&pod.name) {
                        if let Err(e) = on_renew(o, svc).await {
                            problems.push(format!("service {}: on_renew: {e}", svc.name));
                        }
                    }
                }
            }
            Err(e) => problems.push(format!("pod {}: renew failed: {e}", pod.name)),
        }
    }
    match load(p, &pod.name) {
        None => problems.push(format!("pod {}: has no certificate", pod.name)),
        Some(i) if i.expired() => {
            problems.push(format!(
                "pod {}: running on an expired certificate",
                pod.name
            ));
        }
        Some(i) if i.not_after_ms < now_ms() + WARN_BEFORE.as_millis() as u64 => {
            problems.push(format!("pod {}: certificate {}", pod.name, i.describe()));
        }
        Some(_) => {}
    }
    problems
}

/// Tells a service its files changed, as its `on_renew` says.
async fn on_renew(o: &Orchestrator, svc: &Service) -> Result<()> {
    let Some(t) = &svc.tls else { return Ok(()) };
    let e = o.engine(svc.runtime);
    match &t.on_renew {
        OnRenew::None => Ok(()),
        OnRenew::Restart => e.restart(&svc.container_name).await,
        OnRenew::Signal(sig) => e.kill(&svc.container_name, sig).await,
        OnRenew::Exec(cmd) => {
            let code = e
                .exec(
                    &svc.container_name,
                    ExecRequest {
                        cmd: cmd.clone(),
                        tty: false,
                        interactive: false,
                        user: None,
                        workdir: None,
                        env: vec![],
                    },
                )
                .await?;
            if code == 0 {
                Ok(())
            } else {
                Err(Error::engine(format!("{} exited {code}", cmd.join(" "))))
            }
        }
    }
}

/// How often the watcher looks at certificates, and asks each issuer for
/// its own status.
const EVERY: Duration = Duration::from_secs(60);
const STATUS_EVERY: Duration = Duration::from_secs(300);
/// A problem still there is reported again after this long.
const REPEAT: Duration = Duration::from_secs(1800);

/// Keeps every pod certificate on this host fresh until `cancel`: the long
/// running half of pod certificates, run by `gantry supervise` and by
/// library callers such as gantry-agent.
pub async fn watch(cancel: tokio_util::sync::CancellationToken) -> Result<()> {
    let _host = host_lock()?;
    tracing::info!("certificate watcher started");
    let mut last_status = std::time::Instant::now() - STATUS_EVERY;
    let mut reported: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    loop {
        let status_due = last_status.elapsed() >= STATUS_EVERY;
        if status_due {
            last_status = std::time::Instant::now();
        }
        pass(status_due, &mut reported).await;
        reported.retain(|_, t| t.elapsed() < REPEAT * 2);
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(EVERY) => {}
        }
    }
}

/// One look at every project's certificates, as [`watch`] makes each
/// minute: refreshes what is due, asks the issuers for their status when
/// `status` is set, and reports what is wrong. Problems already reported
/// within the last half hour are not repeated.
pub async fn pass(
    status: bool,
    reported: &mut std::collections::HashMap<String, std::time::Instant>,
) {
    let status_due = status;
    {
        for name in state::list_projects() {
            let Ok(Some(project)) = Project::load_snapshot(&name) else {
                continue;
            };
            if !project.pods.values().any(|p| p.tls.is_some()) {
                continue;
            }
            // A gantry command working on the project wins; try next time.
            let Ok(_lock) = crate::orchestrator::lock(&name) else {
                continue;
            };
            let o = Orchestrator::new(project);
            let mut problems = Vec::new();
            for pod in o.project.pods.values().filter(|p| p.tls.is_some()) {
                if state::PodState::load(&name, &pod.name)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    continue;
                }
                problems.extend(refresh_pod(&o, pod).await);
            }
            if status_due {
                let issuers: std::collections::BTreeSet<Option<String>> = o
                    .project
                    .pods
                    .values()
                    .filter_map(|p| p.tls.as_ref().map(|t| t.issuer.clone()))
                    .collect();
                for issuer in issuers {
                    match o
                        .plugins
                        .certificate(issuer.as_deref(), "status", &json!({}))
                        .await
                    {
                        Ok(v) => {
                            for p in v
                                .get("problems")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                            {
                                if let Some(m) = p.get("message").and_then(Value::as_str) {
                                    problems.push(format!("issuer: {m}"));
                                }
                            }
                        }
                        Err(e) => problems.push(format!("issuer status: {e}")),
                    }
                }
            }
            for m in problems {
                let key = format!("{name}: {m}");
                if reported.get(&key).is_some_and(|t| t.elapsed() < REPEAT) {
                    continue;
                }
                reported.insert(key, std::time::Instant::now());
                report(&o, None, &m).await;
            }
        }
    }
}

/// One line per pod with a certificate, for `gantry ps`, and whether any
/// needs attention.
pub fn summary(project: &Project) -> Vec<(String, bool)> {
    project
        .pods
        .values()
        .filter(|p| p.tls.is_some())
        .map(|p| match load(&project.name, &p.name) {
            None => (format!("pod {}: no certificate", p.name), true),
            Some(i) => {
                let soon = i.not_after_ms < now_ms() + WARN_BEFORE.as_millis() as u64;
                (
                    format!("pod {}: certificate {}", p.name, i.describe()),
                    soon,
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(issued_ms: u64, not_after_ms: u64) -> Issued {
        Issued {
            names: vec!["a.example".into()],
            key_type: "ec256".into(),
            key: String::new(),
            cert: String::new(),
            chain: String::new(),
            root: String::new(),
            issued_ms,
            not_after_ms,
        }
    }

    #[test]
    fn due_and_expired() {
        let now = now_ms();
        let hour = 3_600_000;
        // Issued 10 h ago for 24 h: not yet two thirds.
        assert!(!issued(now - 10 * hour, now + 14 * hour).due());
        // Issued 17 h ago for 24 h: due.
        assert!(issued(now - 17 * hour, now + 7 * hour).due());
        assert!(issued(now - 2 * hour, now - 1).expired());
        assert!(
            issued(now - 2 * hour, now - 1)
                .describe()
                .contains("EXPIRED")
        );
    }
}
