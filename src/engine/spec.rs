//! The API create body for a service in its pod. This is where the
//! runtimes differ: how a container joins the pod's namespaces.

use super::{LABEL_HASH, LABEL_POD, LABEL_POD_ID, LABEL_PROJECT, LABEL_SERVICE};
use crate::oci;
use crate::pod::state::PodState;
use crate::project::{MountKind, Project, RuntimeKind, Service};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, HealthConfig, HostConfig, HostConfigLogConfig,
    ResourcesUlimits, RestartPolicy, RestartPolicyNameEnum,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Compose files name images the Docker way: a bare name means Docker
/// Hub. Podman needs the registry spelled out unless short name aliases are
/// configured, so qualify the name for it.
pub fn qualify_image(kind: RuntimeKind, image: &str) -> String {
    if kind != RuntimeKind::Podman || has_registry(image) {
        return image.to_string();
    }
    if image.contains('/') {
        format!("docker.io/{image}")
    } else {
        format!("docker.io/library/{image}")
    }
}

fn has_registry(image: &str) -> bool {
    let first = image.split('/').next().unwrap_or("");
    image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost")
}

/// The image reference a service runs. Podman stores local builds of
/// unqualified names under `localhost/`.
pub fn service_image(svc: &Service) -> String {
    if svc.build.is_some() && svc.runtime == RuntimeKind::Podman && !has_registry(&svc.image) {
        return format!("localhost/{}", svc.image);
    }
    qualify_image(svc.runtime, &svc.image)
}

fn nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

fn bytes(b: u64) -> i64 {
    i64::try_from(b).unwrap_or(i64::MAX)
}

fn restart_policy(r: &str) -> RestartPolicy {
    let (name, count) = match r.split_once(':') {
        Some((n, c)) => (n, c.parse::<i64>().ok()),
        None => (r, None),
    };
    let name = match name {
        "always" => RestartPolicyNameEnum::ALWAYS,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        _ => RestartPolicyNameEnum::NO,
    };
    RestartPolicy {
        name: Some(name),
        maximum_retry_count: count,
    }
}

fn device(spec: &str) -> DeviceMapping {
    let mut parts = spec.splitn(3, ':');
    let host = parts.next().unwrap_or("").to_string();
    let container = parts
        .next()
        .filter(|s| !s.is_empty())
        .map_or_else(|| host.clone(), str::to_string);
    DeviceMapping {
        path_on_host: Some(host),
        path_in_container: Some(container),
        cgroup_permissions: Some(parts.next().unwrap_or("rwm").to_string()),
    }
}

/// Whether this runtime joins the pod's UTS and IPC namespaces by path.
/// kiri needs the second patch in `contrib/kiri` for this.
pub fn joins_uts_ipc(kind: RuntimeKind) -> bool {
    matches!(
        kind,
        RuntimeKind::Docker | RuntimeKind::Podman | RuntimeKind::Kiri
    )
}

/// Builds the create body for a service in its pod. The config hash label
/// is computed over everything else and added last.
pub fn create_spec(project: &Project, svc: &Service, pod: &PodState) -> ContainerCreateBody {
    let mut labels: HashMap<String, String> = svc
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.insert(LABEL_PROJECT.into(), project.name.clone());
    labels.insert(LABEL_SERVICE.into(), svc.name.clone());
    labels.insert(LABEL_POD.into(), svc.pod.clone());
    labels.insert(LABEL_POD_ID.into(), pod.id.clone());

    let mut body = ContainerCreateBody {
        image: Some(service_image(svc)),
        env: Some(
            svc.environment
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect(),
        ),
        working_dir: svc.working_dir.clone(),
        user: svc.user.clone(),
        tty: Some(svc.tty),
        open_stdin: Some(svc.stdin_open),
        stop_signal: svc.stop_signal.clone(),
        stop_timeout: svc
            .stop_grace_period
            .map(|d| i64::try_from(d.as_secs().max(1)).unwrap_or(i64::MAX)),
        cmd: svc.command.clone(),
        // An empty entrypoint clears the image's.
        entrypoint: svc.entrypoint.clone().map(
            |e| {
                if e.is_empty() { vec![String::new()] } else { e }
            },
        ),
        ..Default::default()
    };
    let mut host = HostConfig::default();

    // Joining the pod.
    let netns = pod.netns.display().to_string();
    match svc.runtime {
        RuntimeKind::Docker => {
            host.runtime = Some(super::DOCKER_RUNTIME.into());
            host.network_mode = Some("none".into());
            let mut ann = HashMap::from([(oci::ANN_NETNS.to_string(), netns)]);
            match &pod.uts {
                Some(u) => {
                    ann.insert(oci::ANN_UTSNS.into(), u.display().to_string());
                }
                None => body.hostname = Some(pod.hostname.clone()),
            }
            if let Some(i) = &pod.ipc {
                ann.insert(oci::ANN_IPCNS.into(), i.display().to_string());
            }
            host.annotations = Some(ann);
        }
        RuntimeKind::Podman | RuntimeKind::Kiri => {
            host.network_mode = Some(format!("ns:{netns}"));
            match (&pod.uts, joins_uts_ipc(svc.runtime)) {
                (Some(u), true) => host.uts_mode = Some(format!("ns:{}", u.display())),
                _ => body.hostname = Some(pod.hostname.clone()),
            }
            if let (Some(i), true) = (&pod.ipc, joins_uts_ipc(svc.runtime)) {
                host.ipc_mode = Some(format!("ns:{}", i.display()));
            }
        }
    }

    let mut binds: Vec<String> = crate::pod::etc_mounts(pod)
        .into_iter()
        .filter(|(_, dst)| {
            // Without a shared UTS namespace the engine writes its own
            // /etc/hostname from the hostname we pass.
            !(*dst == "/etc/hostname" && body.hostname.is_some())
        })
        .map(|(src, dst)| format!("{}:{dst}:ro", src.display()))
        .collect();
    let mut anon: Vec<String> = Vec::new();
    let mut tmpfs: HashMap<String, String> = HashMap::new();
    for m in &svc.mounts {
        match m.kind {
            MountKind::Bind | MountKind::Volume => {
                if m.source.is_empty() {
                    anon.push(m.target.clone());
                    continue;
                }
                let mut opts: Vec<String> = Vec::new();
                if m.read_only {
                    opts.push("ro".into());
                }
                opts.extend(m.options.iter().cloned());
                let mut spec = format!("{}:{}", m.source, m.target);
                if !opts.is_empty() {
                    spec.push(':');
                    spec.push_str(&opts.join(","));
                }
                binds.push(spec);
            }
            MountKind::Tmpfs => {
                tmpfs.insert(m.target.clone(), m.options.join(","));
            }
        }
    }
    host.binds = Some(binds);
    if !anon.is_empty() {
        body.volumes = Some(anon);
    }
    if !tmpfs.is_empty() {
        host.tmpfs = Some(tmpfs);
    }

    if let Some(r) = &svc.restart {
        host.restart_policy = Some(restart_policy(r));
    }
    if let Some(h) = &svc.healthcheck {
        body.healthcheck = Some(if h.disabled {
            HealthConfig {
                test: Some(vec!["NONE".into()]),
                ..Default::default()
            }
        } else {
            HealthConfig {
                test: h.test.clone(),
                interval: h.interval.map(nanos),
                timeout: h.timeout.map(nanos),
                retries: h.retries.map(i64::from),
                start_period: h.start_period.map(nanos),
                start_interval: h
                    .start_interval
                    .filter(|_| svc.runtime != RuntimeKind::Podman)
                    .map(nanos),
            }
        });
    }
    if let Some(l) = &svc.logging {
        host.log_config = Some(HostConfigLogConfig {
            typ: l.driver.clone(),
            config: Some(
                l.options
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        });
    }
    let nonempty = |v: &Vec<String>| (!v.is_empty()).then(|| v.clone());
    host.cap_add = nonempty(&svc.cap_add);
    host.cap_drop = nonempty(&svc.cap_drop);
    if !svc.devices.is_empty() {
        host.devices = Some(svc.devices.iter().map(|d| device(d)).collect());
    }
    host.memory = svc.mem_limit.map(bytes);
    host.nano_cpus = svc.cpus.map(|c| (c * 1e9) as i64);
    host.pids_limit = svc.pids_limit;
    host.shm_size = svc.shm_size.map(bytes);
    if !svc.ulimits.is_empty() {
        host.ulimits = Some(
            svc.ulimits
                .iter()
                .map(|(k, (soft, hard))| ResourcesUlimits {
                    name: Some(k.clone()),
                    soft: Some(*soft),
                    hard: Some(*hard),
                })
                .collect(),
        );
    }
    host.security_opt = nonempty(&svc.security_opt);
    host.group_add = nonempty(&svc.group_add);
    if !svc.sysctls.is_empty() {
        host.sysctls = Some(
            svc.sysctls
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
    }
    host.privileged = svc.privileged.then_some(true);
    host.readonly_rootfs = svc.read_only.then_some(true);
    host.init = svc.init.then_some(true);
    body.host_config = Some(host);

    body.labels = Some(labels);
    let hash = config_hash(&body, svc.platform.as_deref());
    if let Some(l) = body.labels.as_mut() {
        l.insert(LABEL_HASH.into(), hash);
    }
    body
}

/// A stable hash of the create body: maps are serialized with sorted keys.
fn config_hash(body: &ContainerCreateBody, platform: Option<&str>) -> String {
    let mut v = serde_json::to_value(body).unwrap_or(Value::Null);
    sort_keys(&mut v);
    let text = serde_json::to_string(&(v, platform)).unwrap_or_default();
    crate::util::hash_hex(text.as_bytes())
}

fn sort_keys(v: &mut Value) {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(m).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, mut val) in entries {
                sort_keys(&mut val);
                m.insert(k, val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

/// Reads the config hash back out of a create body.
pub fn hash_of(body: &ContainerCreateBody) -> String {
    body.labels
        .as_ref()
        .and_then(|l| l.get(LABEL_HASH))
        .cloned()
        .unwrap_or_default()
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
    volumes: ["./data:/data:ro", "/srv/x:/x", "/anon"]
    tmpfs: ["/tmp:size=64m"]
    healthcheck: {{ test: ["CMD", "true"], interval: 5s }}
    restart: on-failure:3
    devices: ["/dev/net/tun"]
    logging: {{ driver: json-file, options: {{ max-size: 10m }} }}
  built:
    build: ./src
    pod: web
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
            netns: PathBuf::from("/run/netns/gt-demo-web"),
            uts: uts.then(|| PathBuf::from("/run/gantry/demo/pods/web/uts")),
            ipc: Some(PathBuf::from("/run/gantry/demo/pods/web/ipc")),
            hostname: "web".into(),
            interfaces: vec![],
            created: 0,
        }
    }

    #[test]
    fn docker_spec() {
        let p = project("docker");
        let b = create_spec(&p, &p.services["app"], &state(true));
        let h = b.host_config.as_ref().unwrap();
        assert_eq!(h.runtime.as_deref(), Some("gantry"));
        assert_eq!(h.network_mode.as_deref(), Some("none"));
        let ann = h.annotations.as_ref().unwrap();
        assert_eq!(ann[oci::ANN_NETNS], "/run/netns/gt-demo-web");
        assert_eq!(ann[oci::ANN_UTSNS], "/run/gantry/demo/pods/web/uts");
        assert!(b.hostname.is_none());
        let binds = h.binds.as_ref().unwrap();
        assert!(binds.iter().any(|s| s.ends_with(":/etc/hosts:ro")));
        assert!(binds.iter().any(|s| s.ends_with(":/data:ro")));
        assert_eq!(b.volumes.as_deref(), Some(&["/anon".to_string()][..]));
        assert_eq!(
            b.entrypoint.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".into()][..])
        );
        assert_eq!(b.cmd.as_deref(), Some(&["echo hi".to_string()][..]));
        let hc = b.healthcheck.as_ref().unwrap();
        assert_eq!(hc.interval, Some(5_000_000_000));
        assert_eq!(h.tmpfs.as_ref().unwrap()["/tmp"], "size=64m");
        let rp = h.restart_policy.as_ref().unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::ON_FAILURE));
        assert_eq!(rp.maximum_retry_count, Some(3));
        assert_eq!(
            h.devices.as_ref().unwrap()[0].path_in_container.as_deref(),
            Some("/dev/net/tun")
        );
        assert_eq!(
            h.log_config.as_ref().unwrap().typ.as_deref(),
            Some("json-file")
        );
        assert!(!hash_of(&b).is_empty());
    }

    #[test]
    fn podman_and_kiri_spec() {
        for rt in ["podman", "kiri"] {
            let p = project(rt);
            let b = create_spec(&p, &p.services["app"], &state(true));
            let h = b.host_config.as_ref().unwrap();
            assert_eq!(h.network_mode.as_deref(), Some("ns:/run/netns/gt-demo-web"));
            assert_eq!(
                h.uts_mode.as_deref(),
                Some("ns:/run/gantry/demo/pods/web/uts")
            );
            assert_eq!(
                h.ipc_mode.as_deref(),
                Some("ns:/run/gantry/demo/pods/web/ipc")
            );
            assert!(b.hostname.is_none());
        }
        // Without a shared UTS namespace the hostname is set directly.
        let p = project("podman");
        let b = create_spec(&p, &p.services["app"], &state(false));
        assert_eq!(b.hostname.as_deref(), Some("web"));
        let binds = b.host_config.unwrap().binds.unwrap();
        assert!(!binds.iter().any(|s| s.contains("/etc/hostname")));
    }

    #[test]
    fn built_images() {
        let p = project("podman");
        assert_eq!(service_image(&p.services["built"]), "localhost/demo-built");
        let p = project("docker");
        assert_eq!(service_image(&p.services["built"]), "demo-built");
        assert!(
            p.services["built"]
                .build
                .as_ref()
                .unwrap()
                .context
                .ends_with("src")
        );
    }

    #[test]
    fn podman_images_are_qualified() {
        assert_eq!(
            qualify_image(RuntimeKind::Podman, "redis:7"),
            "docker.io/library/redis:7"
        );
        assert_eq!(
            qualify_image(RuntimeKind::Podman, "bitnami/redis"),
            "docker.io/bitnami/redis"
        );
        assert_eq!(
            qualify_image(RuntimeKind::Podman, "ghcr.io/a/b:1"),
            "ghcr.io/a/b:1"
        );
        assert_eq!(
            qualify_image(RuntimeKind::Podman, "localhost/x"),
            "localhost/x"
        );
        assert_eq!(qualify_image(RuntimeKind::Docker, "redis:7"), "redis:7");
    }

    #[test]
    fn hash_is_stable_and_tracks_changes() {
        let p = project("docker");
        let a = create_spec(&p, &p.services["app"], &state(true));
        let again = create_spec(&p, &p.services["app"], &state(true));
        assert_eq!(hash_of(&a), hash_of(&again));
        let mut st = state(true);
        st.id = "other".into();
        let b = create_spec(&p, &p.services["app"], &st);
        assert_ne!(hash_of(&a), hash_of(&b));
    }
}
