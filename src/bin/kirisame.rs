//! The `kirisame` command line.

use clap::{Args, Parser, Subcommand};
use kirisame::compose::{LoadOptions, load};
use kirisame::engine::Engine;
use kirisame::errors::{Context, Error, Result};
use kirisame::orchestrator::{Orchestrator, UpOptions, describe_pod};
use kirisame::pod::{self, state};
use kirisame::project::{PullPolicy, RuntimeKind};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

#[derive(Parser)]
#[command(
    name = "kirisame",
    version,
    about = "Pod-first container orchestration with the Compose format"
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args, Clone)]
struct Global {
    /// Compose file(s). Default: kirisame.yaml, compose.yaml or docker-compose.yaml
    #[arg(short = 'f', long = "file", global = true)]
    files: Vec<PathBuf>,
    /// Project name
    #[arg(short = 'p', long = "project-name", global = true)]
    project_name: Option<String>,
    /// Directory relative paths resolve against
    #[arg(long, global = true)]
    project_directory: Option<PathBuf>,
    /// Environment file(s) for interpolation (default: .env)
    #[arg(long = "env-file", global = true)]
    env_files: Vec<PathBuf>,
    /// Activate a profile
    #[arg(long = "profile", global = true)]
    profiles: Vec<String>,
    /// Run every service with this runtime (docker, podman or kiri)
    #[arg(long, global = true, value_parser = parse_runtime)]
    runtime: Option<RuntimeKind>,
    /// Log level (error, warn, info, debug, trace)
    #[arg(long, global = true, default_value = "info", env = "KIRISAME_LOG")]
    log_level: String,
}

fn parse_runtime(s: &str) -> std::result::Result<RuntimeKind, String> {
    RuntimeKind::parse(s).ok_or_else(|| format!("unknown runtime {s:?} (docker, podman, kiri)"))
}

#[derive(Subcommand)]
enum Cmd {
    /// Create pods and start services
    Up(UpArgs),
    /// Stop and remove services and pods
    Down {
        /// Also remove named volumes declared in the file
        #[arg(short = 'v', long)]
        volumes: bool,
        /// Seconds to wait before killing containers
        #[arg(short = 't', long)]
        timeout: Option<u64>,
    },
    /// List services and their state
    Ps,
    /// Show service logs
    Logs {
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long)]
        tail: Option<String>,
        #[arg(short = 't', long)]
        timestamps: bool,
        services: Vec<String>,
    },
    /// Run a command in a running service
    Exec {
        /// Do not allocate a TTY
        #[arg(short = 'T', long = "no-tty")]
        no_tty: bool,
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        service: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Start existing services
    Start { services: Vec<String> },
    /// Stop services (pods stay up)
    Stop {
        #[arg(short = 't', long)]
        timeout: Option<u64>,
        services: Vec<String>,
    },
    /// Restart services
    Restart {
        #[arg(short = 't', long)]
        timeout: Option<u64>,
        services: Vec<String>,
    },
    /// Pull service images
    Pull { services: Vec<String> },
    /// Validate and print the normalized project
    Config {
        /// Print only service names
        #[arg(long)]
        services: bool,
        /// Print only pod names
        #[arg(long)]
        pods: bool,
        /// Print JSON instead of YAML
        #[arg(long)]
        json: bool,
    },
    /// Pod commands
    #[command(subcommand)]
    Pod(PodCmd),
    /// Firewall commands
    #[command(subcommand, alias = "fw")]
    Firewall(FwCmd),
    /// One time host setup for a runtime
    #[command(subcommand)]
    Setup(SetupCmd),
    #[command(subcommand, hide = true)]
    Internal(InternalCmd),
}

#[derive(Args)]
struct UpArgs {
    /// Run in the background
    #[arg(short = 'd', long)]
    detach: bool,
    /// Recreate containers even if their configuration is unchanged
    #[arg(long)]
    force_recreate: bool,
    /// Pull policy for this run (always, missing, never)
    #[arg(long)]
    pull: Option<String>,
    /// Wait until services are running or healthy
    #[arg(long)]
    wait: bool,
    /// Seconds to wait for dependencies and --wait
    #[arg(long)]
    wait_timeout: Option<u64>,
    services: Vec<String>,
}

#[derive(Subcommand)]
enum PodCmd {
    /// List pods and their addresses
    #[command(alias = "ls")]
    List,
    /// Show a pod's state as JSON
    Inspect { pod: String },
    /// Run a host command inside a pod's network namespace
    Exec {
        pod: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum FwCmd {
    /// Print the generated ruleset and what is loaded in the pod
    Show { pod: String },
    /// Re-render and apply every pod's firewall and hosts file
    Reload,
}

#[derive(Subcommand)]
enum SetupCmd {
    /// Register the kirisame-oci runtime shim with Docker
    Docker {
        /// Path to kirisame-oci (default: next to this binary)
        #[arg(long)]
        oci_path: Option<PathBuf>,
        /// Docker daemon config file
        #[arg(long, default_value = "/etc/docker/daemon.json")]
        daemon_json: PathBuf,
        /// Do not reload dockerd afterwards
        #[arg(long)]
        no_reload: bool,
    },
}

#[derive(Subcommand)]
enum InternalCmd {
    /// DHCP agent for one pod interface
    DhcpAgent {
        #[arg(long)]
        project: String,
        #[arg(long)]
        netns: PathBuf,
        #[arg(long)]
        pod_dir: PathBuf,
        #[arg(long)]
        ifname: String,
        #[arg(long, default_value = "")]
        hostname: String,
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        default_route: bool,
        #[arg(long)]
        manage_resolv: bool,
        #[arg(long = "dns-search")]
        dns_search: Vec<String>,
        #[arg(long = "dns-option")]
        dns_options: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter =
        tracing_subscriber::EnvFilter::try_new(format!("kirisame={}", cli.global.log_level))
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("kirisame=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();

    // The DHCP agent must enter the pod namespace before any thread exists,
    // so every thread the runtime starts is inside it.
    if let Cmd::Internal(InternalCmd::DhcpAgent { netns, .. }) = &cli.command {
        if let Err(e) = pod::ns::enter_netns(netns) {
            eprintln!("kirisame: {e}");
            return ExitCode::from(1);
        }
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("kirisame: start runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kirisame: {e}");
            ExitCode::from(1)
        }
    }
}

fn load_project(g: &Global) -> Result<Orchestrator> {
    let opts = LoadOptions {
        files: g.files.clone(),
        project_name: g.project_name.clone(),
        project_dir: g.project_directory.clone(),
        env_files: g.env_files.clone(),
        profiles: g.profiles.clone(),
        runtime: g.runtime,
        cwd: None,
    };
    let loaded = load(&opts)?;
    for w in &loaded.warnings {
        tracing::warn!("{w}");
    }
    Ok(Orchestrator::new(loaded.project))
}

fn require_root() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(Error::State(
            "kirisame runs rootful; run it as root (sudo)".into(),
        ))
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let g = cli.global;
    match cli.command {
        Cmd::Internal(InternalCmd::DhcpAgent {
            project,
            netns: _,
            pod_dir,
            ifname,
            hostname,
            client_id,
            default_route,
            manage_resolv,
            dns_search,
            dns_options,
        }) => {
            let cfg = pod::dhcp::AgentConfig {
                project,
                pod_dir,
                ifname,
                hostname,
                client_id: client_id.into_bytes(),
                default_route,
                manage_resolv,
                dns_search,
                dns_options,
            };
            pod::dhcp::run_agent(cfg).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Config {
            services,
            pods,
            json,
        } => {
            let o = load_project(&g)?;
            if services {
                o.project.services.keys().for_each(|s| println!("{s}"));
            } else if pods {
                o.project.pods.keys().for_each(|p| println!("{p}"));
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&o.project)
                        .map_err(|e| Error::State(e.to_string()))?
                );
            } else {
                print!(
                    "{}",
                    serde_yaml_ng::to_string(&o.project)
                        .map_err(|e| Error::State(e.to_string()))?
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Setup(SetupCmd::Docker {
            oci_path,
            daemon_json,
            no_reload,
        }) => {
            require_root()?;
            setup_docker(oci_path, &daemon_json, !no_reload).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Up(a) => {
            require_root()?;
            let o = load_project(&g)?;
            let pull = a
                .pull
                .as_deref()
                .map(|p| match p {
                    "always" => Ok(PullPolicy::Always),
                    "missing" => Ok(PullPolicy::Missing),
                    "never" => Ok(PullPolicy::Never),
                    o => Err(Error::config(format!(
                        "--pull {o:?} (always, missing, never)"
                    ))),
                })
                .transpose()?;
            let opts = UpOptions {
                services: a.services.clone(),
                force_recreate: a.force_recreate,
                pull,
                wait_timeout: a.wait_timeout.map(Duration::from_secs),
                wait: a.wait,
            };
            o.up(&opts).await?;
            if a.detach {
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "Attaching to logs; press Ctrl-C to stop the services (pods stay up until `kirisame down`)."
            );
            let services = o.start_order(&a.services)?;
            tokio::select! {
                r = logs(&o, &services, true, Some("0".into()), false) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
            println!();
            o.stop(&a.services, None).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Down { volumes, timeout } => {
            require_root()?;
            load_project(&g)?.down(volumes, timeout).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ps => {
            require_root()?;
            let o = load_project(&g)?;
            ps(&o).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Logs {
            follow,
            tail,
            timestamps,
            services,
        } => {
            require_root()?;
            let o = load_project(&g)?;
            let services = if services.is_empty() {
                o.project.services.keys().cloned().collect()
            } else {
                services
            };
            tokio::select! {
                r = logs(&o, &services, follow, tail, timestamps) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Exec {
            no_tty,
            user,
            workdir,
            env,
            service,
            command,
        } => {
            require_root()?;
            let o = load_project(&g)?;
            let svc = o
                .project
                .services
                .get(&service)
                .ok_or_else(|| Error::config(format!("no such service {service:?}")))?;
            let mut cmd = o.engine(svc.runtime).std_cmd();
            cmd.arg("exec");
            let tty = !no_tty && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if tty {
                cmd.arg("-it");
            } else {
                cmd.arg("-i");
            }
            if let Some(u) = user {
                cmd.args(["-u", &u]);
            }
            if let Some(w) = workdir {
                cmd.args(["-w", &w]);
            }
            for e in env {
                cmd.args(["-e", &e]);
            }
            cmd.arg(&svc.container_name).args(&command);
            let status = cmd.status().ctx(|| format!("run {}", svc.runtime))?;
            Ok(ExitCode::from(
                status.code().unwrap_or(1).clamp(0, 255) as u8
            ))
        }
        Cmd::Start { services } => {
            require_root()?;
            load_project(&g)?.start(&services).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Stop { timeout, services } => {
            require_root()?;
            load_project(&g)?.stop(&services, timeout).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restart { timeout, services } => {
            require_root()?;
            let o = load_project(&g)?;
            o.stop(&services, timeout).await?;
            o.start(&services).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pull { services } => {
            require_root()?;
            let o = load_project(&g)?;
            let list = if services.is_empty() {
                o.project.services.keys().cloned().collect()
            } else {
                services
            };
            for s in list {
                let svc = o
                    .project
                    .services
                    .get(&s)
                    .ok_or_else(|| Error::config(format!("no such service {s:?}")))?;
                println!("Pulling {} for {s} ({})", svc.image, svc.runtime);
                o.engine(svc.runtime).pull(&svc.image).await?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pod(PodCmd::List) => {
            require_root()?;
            let o = load_project(&g)?;
            pod_list(&o);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pod(PodCmd::Inspect { pod }) => {
            require_root()?;
            let o = load_project(&g)?;
            let st = state::PodState::load(&o.project.name, &pod)?
                .ok_or_else(|| Error::State(format!("pod {pod} is not up")))?;
            let mut v = serde_json::to_value(&st).map_err(|e| Error::State(e.to_string()))?;
            let leases: Vec<_> = st
                .interfaces
                .iter()
                .filter(|i| i.dhcp)
                .filter_map(|i| {
                    pod::dhcp::LeaseFile::load(&state::pod_dir(&st.project, &st.name), &i.ifname)
                        .ok()
                        .flatten()
                })
                .collect();
            v["dhcp_leases"] = serde_json::to_value(leases).unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Pod(PodCmd::Exec { pod, command }) => {
            require_root()?;
            let o = load_project(&g)?;
            let st = state::PodState::load(&o.project.name, &pod)?
                .ok_or_else(|| Error::State(format!("pod {pod} is not up")))?;
            let mut cmd = std::process::Command::new(&command[0]);
            cmd.args(&command[1..]);
            pod::ns::command_in_netns(&mut cmd, &st.netns)?;
            let status = cmd.status().ctx(|| format!("run {}", command[0]))?;
            Ok(ExitCode::from(
                status.code().unwrap_or(1).clamp(0, 255) as u8
            ))
        }
        Cmd::Firewall(FwCmd::Show { pod }) => {
            require_root()?;
            let o = load_project(&g)?;
            let st = state::PodState::load(&o.project.name, &pod)?
                .ok_or_else(|| Error::State(format!("pod {pod} is not up")))?;
            let path = state::pod_dir(&o.project.name, &pod).join("firewall.nft");
            match std::fs::read_to_string(&path) {
                Ok(t) => println!("# {}\n{t}", path.display()),
                Err(_) => println!("# pod {pod} has no firewall section; all traffic is allowed"),
            }
            let out = pod::nft_output(&st.netns, &["list", "ruleset"])?;
            println!(
                "# loaded in the pod namespace:\n{}",
                String::from_utf8_lossy(&out.stdout)
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Firewall(FwCmd::Reload) => {
            require_root()?;
            let o = load_project(&g)?;
            pod::refresh(&o.project)?;
            println!("Firewalls and hosts files reloaded");
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn ps(o: &Orchestrator) -> Result<()> {
    let containers = o.containers().await;
    let mut rows = vec![
        ["SERVICE", "POD", "RUNTIME", "STATE", "CONTAINER"]
            .map(String::from)
            .to_vec(),
    ];
    for svc in o.project.services.values() {
        let found = containers
            .iter()
            .find(|(e, c)| c.service == svc.name && e.kind == svc.runtime);
        let state = match found {
            Some((_, c)) => match &c.health {
                Some(h) if c.status == "running" => format!("{} ({h})", c.status),
                Some(_) => c.status.clone(),
                None => c.status.clone(),
            },
            None => "not created".into(),
        };
        rows.push(vec![
            svc.name.clone(),
            svc.pod.clone(),
            svc.runtime.to_string(),
            state,
            svc.container_name.clone(),
        ]);
    }
    for (e, c) in &containers {
        if !o.project.services.contains_key(&c.service) {
            rows.push(vec![
                c.service.clone(),
                c.pod.clone(),
                e.kind.to_string(),
                format!("{} (orphan)", c.status),
                c.name.clone(),
            ]);
        }
    }
    print_table(&rows);
    Ok(())
}

fn pod_list(o: &Orchestrator) {
    let mut rows = vec![
        ["POD", "NETNS", "NETWORK", "FIREWALL", "SERVICES"]
            .map(String::from)
            .to_vec(),
    ];
    for (name, p) in &o.project.pods {
        let services: Vec<String> = o
            .project
            .services_in_pod(name)
            .map(|s| s.name.clone())
            .collect();
        let fw = if p.firewall.is_some() { "on" } else { "off" };
        match state::PodState::load(&o.project.name, name).ok().flatten() {
            Some(st) => rows.push(vec![
                name.clone(),
                state::netns_name(&o.project.name, name),
                describe_pod(&st),
                fw.into(),
                services.join(","),
            ]),
            None => rows.push(vec![
                name.clone(),
                "-".into(),
                "not created".into(),
                fw.into(),
                services.join(","),
            ]),
        }
    }
    print_table(&rows);
}

fn print_table(rows: &[Vec<String>]) {
    let cols = rows.first().map(Vec::len).unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i + 1 == cols {
                    c.clone()
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect();
        println!("{}", line.join("   "));
    }
}

/// Streams logs from several services, each line prefixed with its name.
async fn logs(
    o: &Orchestrator,
    services: &[String],
    follow: bool,
    tail: Option<String>,
    timestamps: bool,
) -> Result<()> {
    let width = services.iter().map(String::len).max().unwrap_or(0);
    let mut tasks = Vec::new();
    for name in services {
        let svc = o
            .project
            .services
            .get(name)
            .ok_or_else(|| Error::config(format!("no such service {name:?}")))?;
        let mut cmd = tokio::process::Command::from(o.engine(svc.runtime).std_cmd());
        cmd.arg("logs");
        if follow {
            cmd.arg("-f");
        }
        if let Some(t) = &tail {
            cmd.args(["--tail", t]);
        }
        if timestamps {
            cmd.arg("-t");
        }
        cmd.arg(&svc.container_name)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().ctx(|| format!("{} logs", svc.runtime))?;
        let prefix = format!("{name:<width$} | ");
        let out = child.stdout.take();
        let err = child.stderr.take();
        tasks.push(tokio::spawn(async move {
            let p2 = prefix.clone();
            let a = async {
                if let Some(o) = out {
                    let mut lines = tokio::io::BufReader::new(o).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        println!("{prefix}{l}");
                    }
                }
            };
            let b = async {
                if let Some(e) = err {
                    let mut lines = tokio::io::BufReader::new(e).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        println!("{p2}{l}");
                    }
                }
            };
            tokio::join!(a, b);
            let _ = child.wait().await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

async fn setup_docker(
    oci_path: Option<PathBuf>,
    daemon_json: &std::path::Path,
    reload: bool,
) -> Result<()> {
    let oci = match oci_path {
        Some(p) => p,
        None => std::env::current_exe()
            .ctx(|| "locate kirisame".into())?
            .with_file_name("kirisame-oci"),
    };
    let oci = std::fs::canonicalize(&oci).ctx(|| format!("find {}", oci.display()))?;
    let mut cfg: serde_json::Value = match std::fs::read_to_string(daemon_json) {
        Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t)
            .map_err(|e| Error::config(format!("{}: {e}", daemon_json.display())))?,
        Ok(_) => serde_json::json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).ctx(|| format!("read {}", daemon_json.display())),
    };
    let obj = cfg
        .as_object_mut()
        .ok_or_else(|| Error::config(format!("{} is not a JSON object", daemon_json.display())))?;
    let runtimes = obj
        .entry("runtimes")
        .or_insert_with(|| serde_json::json!({}));
    runtimes[kirisame::engine::DOCKER_RUNTIME] =
        serde_json::json!({ "path": oci.display().to_string() });
    if let Some(dir) = daemon_json.parent() {
        state::ensure_dir(dir)?;
    }
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| Error::State(e.to_string()))?;
    kirisame::util::write_atomic(daemon_json, format!("{text}\n").as_bytes())?;
    println!(
        "Registered runtime {:?} -> {} in {}",
        kirisame::engine::DOCKER_RUNTIME,
        oci.display(),
        daemon_json.display()
    );
    if reload {
        let status = std::process::Command::new("systemctl")
            .args(["reload", "docker"])
            .status();
        match status {
            Ok(s) if s.success() => println!("Reloaded dockerd"),
            _ => println!(
                "Could not reload dockerd; run `systemctl reload docker` (or send it SIGHUP)"
            ),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        Engine::new(RuntimeKind::Docker).check().await?;
        println!("Docker is ready for kirisame pods");
    }
    Ok(())
}
