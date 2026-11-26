//! `gantry stats`: resource use per container and per pod.
//!
//! A pod is a set of namespaces, not a cgroup. Each container keeps its own
//! cgroup, so CPU, memory, processes and block I/O are read per container
//! from its engine and summed for the pod. The containers of a pod share
//! one network namespace, so network traffic is read once per pod from its
//! interfaces; a per-container figure would repeat the pod's.

use crate::engine::{ContainerInfo, Engine, Sample};
use crate::errors::Result;
use crate::orchestrator::Orchestrator;
use crate::pod::{self, state};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContainerStats {
    pub service: String,
    pub name: String,
    pub engine: String,
    pub state: String,
    /// Percent of one CPU (200 is two full CPUs).
    pub cpu_percent: Option<f64>,
    /// The service's `cpus`, when it has a limit.
    pub cpu_limit: Option<f64>,
    /// Percent of scheduler periods in which the CPU limit held it back.
    pub throttled_percent: Option<f64>,
    pub memory: Option<u64>,
    pub memory_limit: Option<u64>,
    pub pids: Option<u64>,
    pub block_read_rate: Option<f64>,
    pub block_write_rate: Option<f64>,
    /// Why the engine had no figures for it, such as an engine that does
    /// not report stats.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PodStats {
    pub pod: String,
    pub cpu_percent: f64,
    pub memory: u64,
    pub pids: u64,
    /// Bytes per second across the pod's interfaces, loopback excluded.
    pub net_rx_rate: Option<f64>,
    pub net_tx_rate: Option<f64>,
    pub containers: Vec<ContainerStats>,
}

/// Reads every running service container twice, `interval` apart, and the
/// pods' interface counters at the same moments.
pub async fn collect(
    o: &Orchestrator,
    services: &[String],
    interval: Duration,
) -> Result<Vec<PodStats>> {
    let p = &o.project;
    let mut containers: Vec<(Engine, ContainerInfo)> = o
        .containers()
        .await?
        .into_iter()
        .filter(|(_, c)| c.project == p.name && !c.oneoff)
        .filter(|(_, c)| services.is_empty() || services.contains(&c.service))
        .collect();
    containers.sort_by(|a, b| (&a.1.pod, &a.1.name).cmp(&(&b.1.pod, &b.1.name)));
    let pods: Vec<String> = {
        let mut v: Vec<String> = containers.iter().map(|(_, c)| c.pod.clone()).collect();
        v.dedup();
        v
    };

    let take = || async {
        let samples = futures_util::future::join_all(containers.iter().map(|(e, c)| async move {
            if c.status == "running" {
                e.sample(&c.id).await.map_err(|e| e.to_string())
            } else {
                Err(String::new())
            }
        }))
        .await;
        let net: Vec<Option<(u64, u64)>> = pods.iter().map(|pod| pod_net(&p.name, pod)).collect();
        (Instant::now(), samples, net)
    };
    let (t0, first, net0) = take().await;
    tokio::time::sleep(interval).await;
    let (t1, second, net1) = take().await;
    let secs = t1.duration_since(t0).as_secs_f64().max(0.001);

    let mut by_pod: BTreeMap<&str, Vec<ContainerStats>> = BTreeMap::new();
    for (i, (e, c)) in containers.iter().enumerate() {
        let svc = p.services.get(&c.service);
        let cpu_limit = svc.and_then(|s| s.cpus).filter(|c| *c > 0.0);
        let row = match (&first[i], &second[i]) {
            (Ok(a), Ok(b)) => rates(a, b, secs, cpu_limit),
            (_, Err(err)) | (Err(err), _) => ContainerStats {
                cpu_limit,
                error: (!err.is_empty()).then(|| err.clone()),
                ..empty()
            },
        };
        by_pod.entry(&c.pod).or_default().push(ContainerStats {
            service: c.service.clone(),
            name: c.name.clone(),
            engine: e.kind.as_str().to_string(),
            state: c.status.clone(),
            ..row
        });
    }

    Ok(pods
        .iter()
        .enumerate()
        .map(|(i, pod)| {
            let containers = by_pod.remove(pod.as_str()).unwrap_or_default();
            let (rx, tx) = match (net0[i], net1[i]) {
                (Some(a), Some(b)) => (
                    Some(b.0.saturating_sub(a.0) as f64 / secs),
                    Some(b.1.saturating_sub(a.1) as f64 / secs),
                ),
                _ => (None, None),
            };
            PodStats {
                pod: pod.clone(),
                cpu_percent: containers.iter().filter_map(|c| c.cpu_percent).sum(),
                memory: containers.iter().filter_map(|c| c.memory).sum(),
                pids: containers.iter().filter_map(|c| c.pids).sum(),
                net_rx_rate: rx,
                net_tx_rate: tx,
                containers,
            }
        })
        .collect())
}

fn empty() -> ContainerStats {
    ContainerStats {
        service: String::new(),
        name: String::new(),
        engine: String::new(),
        state: String::new(),
        cpu_percent: None,
        cpu_limit: None,
        throttled_percent: None,
        memory: None,
        memory_limit: None,
        pids: None,
        block_read_rate: None,
        block_write_rate: None,
        error: None,
    }
}

/// The figures for one container from two readings `secs` apart.
fn rates(a: &Sample, b: &Sample, secs: f64, cpu_limit: Option<f64>) -> ContainerStats {
    let cpu_percent = match (a.cpu_ns, b.cpu_ns) {
        (Some(x), Some(y)) => Some(y.saturating_sub(x) as f64 / (secs * 1e9) * 100.0),
        _ => None,
    };
    let periods = b.periods.saturating_sub(a.periods);
    let throttled_percent = (cpu_limit.is_some() && periods > 0).then(|| {
        b.throttled_periods.saturating_sub(a.throttled_periods) as f64 / periods as f64 * 100.0
    });
    // Without a limit the engine reports the host's memory; say none.
    let memory_limit = b
        .memory_limit
        .filter(|l| *l < host_memory().unwrap_or(u64::MAX));
    ContainerStats {
        cpu_percent,
        cpu_limit,
        throttled_percent,
        memory: b.memory,
        memory_limit,
        pids: b.pids,
        block_read_rate: Some(b.block_read.saturating_sub(a.block_read) as f64 / secs),
        block_write_rate: Some(b.block_write.saturating_sub(a.block_write) as f64 / secs),
        ..empty()
    }
}

fn host_memory() -> Option<u64> {
    let info = nix::sys::sysinfo::sysinfo().ok()?;
    Some(info.ram_total())
}

/// Bytes received and sent on the pod's interfaces, loopback excluded.
fn pod_net(project: &str, pod: &str) -> Option<(u64, u64)> {
    let st = state::PodState::load(project, pod).ok()??;
    let text = pod::ns::in_netns(&st.netns, || {
        std::fs::read_to_string("/proc/thread-self/net/dev")
            .map_err(|e| crate::Error::network(format!("read net/dev: {e}")))
    })
    .ok()?;
    Some(parse_net_dev(&text))
}

/// Sums received and sent bytes in `/proc/net/dev`, without `lo`.
fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut rx = 0;
    let mut tx = 0;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if f.len() >= 9 {
            rx += f[0];
            tx += f[8];
        }
    }
    (rx, tx)
}

/// The table `gantry stats` prints.
pub fn render(pods: &[PodStats]) -> String {
    let mut rows: Vec<[String; 9]> = vec![[
        "POD / CONTAINER".into(),
        "ENGINE".into(),
        "CPU %".into(),
        "CPUS".into(),
        "THROTTLED".into(),
        "MEM USAGE / LIMIT".into(),
        "PIDS".into(),
        "BLOCK READ / WRITE".into(),
        "NET RX / TX".into(),
    ]];
    for p in pods {
        let net = match (p.net_rx_rate, p.net_tx_rate) {
            (Some(r), Some(t)) => format!("{} / {}", rate(r), rate(t)),
            _ => "-".into(),
        };
        rows.push([
            p.pod.clone(),
            String::new(),
            format!("{:.1}%", p.cpu_percent),
            String::new(),
            String::new(),
            bytes(p.memory),
            p.pids.to_string(),
            String::new(),
            net,
        ]);
        for c in &p.containers {
            let dash = || "-".to_string();
            let name = if c.state == "running" {
                format!("  {}", c.name)
            } else {
                format!("  {} ({})", c.name, c.state)
            };
            rows.push([
                name,
                c.engine.clone(),
                c.cpu_percent.map_or_else(dash, |v| format!("{v:.1}%")),
                c.cpu_limit.map_or_else(dash, |v| format!("{v}")),
                c.throttled_percent
                    .map_or_else(dash, |v| format!("{v:.0}%")),
                match (c.memory, c.memory_limit) {
                    (Some(m), Some(l)) => format!("{} / {}", bytes(m), bytes(l)),
                    (Some(m), None) => bytes(m),
                    _ => dash(),
                },
                c.pids.map_or_else(dash, |v| v.to_string()),
                match (c.block_read_rate, c.block_write_rate) {
                    (Some(r), Some(w)) => format!("{} / {}", rate(r), rate(w)),
                    _ => dash(),
                },
                String::new(),
            ]);
        }
    }
    let widths: Vec<usize> = (0..9)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in &rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{v:<w$}", w = widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    let errors: Vec<String> = pods
        .iter()
        .flat_map(|p| &p.containers)
        .filter_map(|c| c.error.as_ref().map(|e| format!("{}: {e}", c.name)))
        .collect();
    for e in errors {
        out.push_str(&format!("note: {e}\n"));
    }
    out
}

/// Binary units, as `docker stats` prints them.
fn bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", units[i])
    }
}

fn rate(bps: f64) -> String {
    format!("{}/s", bytes(bps.max(0.0).round() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_dev() {
        let text = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:  900 10 0 0 0 0 0 0  900 10 0 0 0 0 0 0\n  eth0: 1000 10 0 0 0 0 0 0  2000 20 0 0 0 0 0 0\n  eth1:   24  1 0 0 0 0 0 0    76  1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(text), (1024, 2076));
    }

    #[test]
    fn rates_from_two_samples() {
        let a = Sample {
            cpu_ns: Some(1_000_000_000),
            block_read: 0,
            block_write: 1000,
            periods: 100,
            throttled_periods: 10,
            ..Default::default()
        };
        let b = Sample {
            cpu_ns: Some(1_500_000_000),
            memory: Some(1024),
            memory_limit: Some(2048),
            pids: Some(3),
            block_read: 2048,
            block_write: 1000,
            periods: 200,
            throttled_periods: 35,
        };
        let r = rates(&a, &b, 1.0, Some(1.0));
        assert_eq!(r.cpu_percent, Some(50.0));
        assert_eq!(r.throttled_percent, Some(25.0));
        assert_eq!(r.block_read_rate, Some(2048.0));
        assert_eq!(r.block_write_rate, Some(0.0));
        assert_eq!(
            (r.memory, r.memory_limit, r.pids),
            (Some(1024), Some(2048), Some(3))
        );
        // No CPU limit: throttling is not meaningful.
        assert_eq!(rates(&a, &b, 1.0, None).throttled_percent, None);
    }

    #[test]
    fn units() {
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2.0GiB");
        assert_eq!(rate(1536.0), "1.5KiB/s");
    }
}
