//! The `gantry` command line.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::{Args, Parser, Subcommand};
use gantry::compose::{LoadOptions, load};
use gantry::engine::Engine;
use gantry::engine::exec::{ExecRequest, RunRequest};
use gantry::errors::{Context, Error, Result};
use gantry::orchestrator::{Orchestrator, UpOptions, describe_pod};
use gantry::pod::{self, state};
use gantry::project::{PullPolicy, RuntimeKind};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "gantry",
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
    /// Compose file(s). Default: gantry.yaml, compose.yaml or docker-compose.yaml
    #[arg(short = 'f', long = "file", global = true)]
    files: Vec<PathBuf>,
    /// Project name
    #[arg(short = 'p', long = "project-name", global = true)]
    project_name: Option<String>,
    /// Directory relative paths resolve against
    #[arg(long, global = true)]
    project_directory: Option<PathBuf>,
    /// Use the installed project NAME, found by name in the directories
    /// listed in /etc/gantry/project-dirs (default /etc/gantry)
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        conflicts_with = "project_directory"
    )]
    installed: Option<String>,
    /// Environment file(s) for interpolation (default: .env)
    #[arg(long = "env-file", global = true)]
    env_files: Vec<PathBuf>,
    /// Activate a profile
    #[arg(long = "profile", global = true)]
    profiles: Vec<String>,
    /// Run every service with this runtime (docker or kiri)
    #[arg(long, global = true, value_parser = parse_runtime)]
    runtime: Option<RuntimeKind>,
    /// Log level (error, warn, info, debug, trace)
    #[arg(long, global = true, default_value = "info", env = "GANTRY_LOG")]
    log_level: String,
}

fn parse_runtime(s: &str) -> std::result::Result<RuntimeKind, String> {
    RuntimeKind::parse(s).ok_or_else(|| format!("unknown runtime {s:?} (docker, kiri)"))
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
    /// Run a one-off command in a new container of a service, in its pod
    Run {
        /// Remove the container when it exits
        #[arg(long)]
        rm: bool,
        /// Do not allocate a TTY
        #[arg(short = 'T', long = "no-tty")]
        no_tty: bool,
        /// Do not start the service's dependencies
        #[arg(long)]
        no_deps: bool,
        /// Container name (default: <project>-<service>-run-<id>)
        #[arg(long)]
        name: Option<String>,
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Override the entrypoint (one word; use the command for arguments)
        #[arg(long)]
        entrypoint: Option<String>,
        service: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Send a signal to services (default SIGKILL)
    Kill {
        #[arg(short = 's', long, default_value = "SIGKILL")]
        signal: String,
        services: Vec<String>,
    },
    /// Remove stopped service containers (pods stay)
    Rm {
        /// Stop the services first
        #[arg(short = 's', long)]
        stop: bool,
        /// Accepted for Compose compatibility; gantry never prompts
        #[arg(short = 'f', long)]
        force: bool,
        services: Vec<String>,
    },
    /// Show the processes running in services
    Top { services: Vec<String> },
    /// List projects that have pods on this host
    Ls,
    /// Show gantry and engine versions
    Version,
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
    /// Build service images that have a build section
    Build { services: Vec<String> },
    /// Validate and print the normalized project, with likely secrets redacted
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
    /// Service data commands
    #[command(subcommand)]
    Data(DataCmd),
    /// Projects started at boot, in pod priority order
    #[command(subcommand)]
    Boot(BootCmd),
    /// One time host setup for a runtime
    #[command(subcommand)]
    Setup(SetupCmd),
    /// Watch every engine and act on unhealthy containers that set
    /// `health_on_failure` (runs until stopped; see gantry-supervise.service)
    Supervise,
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    Completion { shell: clap_complete::Shell },
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
    /// Build images before starting, even when they exist
    #[arg(long)]
    build: bool,
    /// Seconds to wait for dependencies and --wait
    #[arg(long)]
    wait_timeout: Option<u64>,
    services: Vec<String>,
}

#[derive(Subcommand)]
enum BootCmd {
    /// Show the registered projects and the order boot starts their pods
    #[command(alias = "list")]
    Ls,
    /// Start every registered project, one pod priority level at a time
    /// (run by gantry-boot.service)
    Start {
        /// Seconds to wait for each level to be running or healthy
        #[arg(long, default_value_t = 300)]
        wait_timeout: u64,
    },
    /// Stop every registered project in reverse priority order and take
    /// its pods down; registrations are kept (run at shutdown)
    Stop,
}

#[derive(Subcommand)]
enum DataCmd {
    /// Move data to the paths the project file now names, then recreate
    /// the services that use it
    ///
    /// Change the path in the project file or .env first. The data is
    /// copied with rsync while the services run, the services are stopped
    /// for a final pass and a checksum check, then recreated on the new
    /// path. The old data is kept.
    Move {
        /// Show what would move and stop
        #[arg(long)]
        dry_run: bool,
        /// Live copy passes before the services stop
        #[arg(long, default_value_t = 3)]
        passes: u32,
        /// Do not copy ACLs and extended attributes (for filesystems
        /// without them)
        #[arg(long)]
        no_xattrs: bool,
        /// Allow a destination that already holds data (it is replaced)
        #[arg(long)]
        force: bool,
        /// Seconds to wait for each service to stop
        #[arg(short = 't', long)]
        timeout: Option<u64>,
        services: Vec<String>,
    },
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
    /// Show each pod's loaded rules with hit counts (all pods by default)
    Show {
        /// Only this pod
        pod: Option<String>,
        /// Print the generated ruleset and the full nft listing instead
        #[arg(long)]
        raw: bool,
    },
    /// Re-render and apply every pod's firewall and hosts file
    Reload,
}

#[derive(Subcommand)]
enum SetupCmd {
    /// Register the gantry-oci runtime shim with Docker
    Docker {
        /// Path to gantry-oci (default: next to this binary)
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
    /// Print the man page
    Man,
    /// Print the directory of an installed project
    Locate { name: String },
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
    // Exit quietly when stdout is a closed pipe (`gantry ps | head`) instead
    // of panicking in println!.
    // SAFETY: setting a signal disposition to the default before any other
    // thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_new(format!("gantry={}", cli.global.log_level))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gantry=info"));
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
            eprintln!("gantry: {e}");
            return ExitCode::from(1);
        }
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gantry: start runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gantry: {e}");
            // Exit without dropping the runtime: an interactive session may
            // have left a stdin read blocked that would hold up shutdown.
            std::process::exit(1);
        }
    }
}

fn load_project(g: &Global) -> Result<Orchestrator> {
    let installed = g
        .installed
        .as_deref()
        .map(gantry::installed::locate)
        .transpose()?;
    let opts = LoadOptions {
        files: g.files.clone(),
        project_name: g.project_name.clone(),
        project_dir: g.project_directory.clone(),
        env_files: g.env_files.clone(),
        profiles: g.profiles.clone(),
        runtime: g.runtime,
        cwd: installed,
    };
    let loaded = load(&opts)?;
    for w in &loaded.warnings {
        tracing::warn!("{w}");
    }
    // Every command acts on pods by project name, so a name already running
    // from another directory is refused up front.
    gantry::orchestrator::guard_project_name(&loaded.project)?;
    Ok(Orchestrator::new(loaded.project))
}

fn require_root() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(Error::State(
            "gantry runs rootful; run it as root (sudo)".into(),
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
        Cmd::Completion { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "gantry", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Internal(InternalCmd::Locate { name }) => {
            println!("{}", gantry::installed::locate(&name)?.display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Internal(InternalCmd::Man) => {
            use clap::CommandFactory;
            clap_mangen::Man::new(Cli::command())
                .render(&mut std::io::stdout())
                .ctx(|| "render man page".into())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Config {
            services,
            pods,
            json,
        } => {
            let o = load_project(&g)?;
            let shown = gantry::redact::redacted(&o.project);
            if services {
                o.project.services.keys().for_each(|s| println!("{s}"));
            } else if pods {
                o.project.pods.keys().for_each(|p| println!("{p}"));
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&shown)
                        .map_err(|e| Error::State(e.to_string()))?
                );
            } else {
                print!(
                    "{}",
                    serde_norway::to_string(&shown).map_err(|e| Error::State(e.to_string()))?
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
                    "build" => Ok(PullPolicy::Build),
                    o => Err(Error::config(format!(
                        "--pull {o:?} (always, missing, never, build)"
                    ))),
                })
                .transpose()?;
            let opts = UpOptions {
                services: a.services.clone(),
                force_recreate: a.force_recreate,
                pull,
                wait_timeout: a.wait_timeout.map(Duration::from_secs),
                wait: a.wait,
                build: a.build,
            };
            o.up(&opts).await?;
            if a.detach {
                // Installed projects come back at boot (gantry-boot.service).
                if let Some(name) = gantry::boot::register(&o.project.dir)? {
                    println!("Registered {name} to start at boot");
                }
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "Attaching to logs; press Ctrl-C to stop the services (pods stay up until `gantry down`)."
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
            let o = load_project(&g)?;
            o.down(volumes, timeout).await?;
            if let Some(name) = gantry::boot::unregister(&o.project.dir)? {
                println!("Removed {name} from boot");
            }
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
            let tty = !no_tty && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            let req = ExecRequest {
                cmd: command,
                tty,
                interactive: tty || !std::io::stdin().is_terminal(),
                user,
                workdir,
                env: expand_env(env),
            };
            let code = o.engine(svc.runtime).exec(&svc.container_name, req).await?;
            // Exit directly: the stdin reader may still be blocked in a read.
            std::process::exit(gantry::engine::exec::exit_status(code));
        }
        Cmd::Run {
            rm,
            no_tty,
            no_deps,
            name,
            user,
            workdir,
            env,
            entrypoint,
            service,
            command,
        } => {
            require_root()?;
            let o = load_project(&g)?;
            let stdin_tty = std::io::stdin().is_terminal();
            let tty = !no_tty && stdin_tty && std::io::stdout().is_terminal();
            let req = RunRequest {
                cmd: command,
                entrypoint: entrypoint.map(|e| vec![e]),
                tty,
                interactive: tty || !stdin_tty,
                user,
                workdir,
                env: expand_env(env),
            };
            let code = o.run_oneoff(&service, &req, name, rm, no_deps).await?;
            // Exit directly: the stdin reader may still be blocked in a read.
            std::process::exit(gantry::engine::exec::exit_status(code));
        }
        Cmd::Kill { signal, services } => {
            require_root()?;
            load_project(&g)?.kill(&services, &signal).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Rm {
            stop,
            force: _,
            services,
        } => {
            require_root()?;
            load_project(&g)?.rm(&services, stop).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Top { services } => {
            require_root()?;
            let o = load_project(&g)?;
            for name in o.selected(&services)? {
                let svc = &o.project.services[&name];
                match o.engine(svc.runtime).top(&svc.container_name).await {
                    Ok((titles, procs)) => {
                        println!("{name} ({})", svc.container_name);
                        let mut rows = vec![titles];
                        rows.extend(procs);
                        print_table(&rows);
                        println!();
                    }
                    Err(e) => println!("{name}: {e}\n"),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ls => {
            require_root()?;
            list_projects();
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Version => {
            println!("gantry {}", env!("CARGO_PKG_VERSION"));
            println!("features: {}", gantry::FEATURES.join(", "));
            for kind in [RuntimeKind::Docker, RuntimeKind::Kiri] {
                let e = Engine::new(kind);
                match e.version().await {
                    Ok(v) => println!("{kind}: {v}"),
                    Err(_) => println!("{kind}: not available"),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Supervise => {
            require_root()?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let stop = cancel.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut term), Ok(mut int)) = (
                    signal(SignalKind::terminate()),
                    signal(SignalKind::interrupt()),
                ) else {
                    return;
                };
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
                stop.cancel();
            });
            gantry::heal::watch(
                &[RuntimeKind::Docker, RuntimeKind::Kiri],
                gantry::heal::Policy::default(),
                gantry::heal::Control::default(),
                None,
                cancel,
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Start { services } => {
            require_root()?;
            load_project(&g)?.start(&services).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Boot(BootCmd::Ls) => {
            print!("{}", gantry::boot::plan()?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Boot(BootCmd::Start { wait_timeout }) => {
            require_root()?;
            let failed = gantry::boot::start(Duration::from_secs(wait_timeout)).await;
            // 2: some projects failed, the rest are up (SuccessExitStatus in
            // the unit, so shutdown still runs `boot stop`).
            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Cmd::Boot(BootCmd::Stop) => {
            require_root()?;
            let failed = gantry::boot::stop().await;
            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Cmd::Data(DataCmd::Move {
            dry_run,
            passes,
            no_xattrs,
            force,
            timeout,
            services,
        }) => {
            require_root()?;
            let o = load_project(&g)?;
            gantry::datamove::run(
                &o,
                &gantry::datamove::MoveOptions {
                    services,
                    passes,
                    dry_run,
                    xattrs: !no_xattrs,
                    force,
                    timeout,
                },
            )
            .await?;
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
            let mut svcs = Vec::new();
            for s in &list {
                let svc = o
                    .project
                    .services
                    .get(s)
                    .ok_or_else(|| Error::config(format!("no such service {s:?}")))?;
                if svc.build.is_none() {
                    svcs.push(svc);
                }
            }
            futures_util::future::try_join_all(svcs.iter().map(|svc| {
                let o = &o;
                async move {
                    println!("Pulling {} for {} ({})", svc.image, svc.name, svc.runtime);
                    let t = std::time::Instant::now();
                    o.engine(svc.runtime)
                        .pull(&svc.image, svc.platform.as_deref())
                        .await?;
                    println!("Pulled {} in {:.1}s", svc.image, t.elapsed().as_secs_f32());
                    Ok::<(), Error>(())
                }
            }))
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Build { services } => {
            require_root()?;
            let o = load_project(&g)?;
            let list: Vec<String> = if services.is_empty() {
                o.project
                    .services
                    .iter()
                    .filter(|(_, s)| s.build.is_some())
                    .map(|(n, _)| n.clone())
                    .collect()
            } else {
                services
            };
            for s in &list {
                let svc = o
                    .project
                    .services
                    .get(s)
                    .ok_or_else(|| Error::config(format!("no such service {s:?}")))?;
                if svc.build.is_none() {
                    return Err(Error::config(format!("service {s} has no build section")));
                }
            }
            o.ensure_images(&list, None, true).await?;
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
        Cmd::Firewall(FwCmd::Show { pod, raw }) => {
            require_root()?;
            let o = load_project(&g)?;
            let project = &o.project;
            let pods: Vec<String> = match pod {
                Some(p) if project.pods.contains_key(&p) => vec![p],
                Some(p) => {
                    return Err(Error::config(format!(
                        "project {} has no pod {p}",
                        project.name
                    )));
                }
                None => project.pods.keys().cloned().collect(),
            };
            for (i, pod) in pods.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                let Some(st) = state::PodState::load(&project.name, pod)? else {
                    println!("pod {pod}: not up");
                    continue;
                };
                if raw {
                    let path = state::pod_dir(&project.name, pod).join("firewall.nft");
                    match std::fs::read_to_string(&path) {
                        Ok(t) => println!("# {}\n{t}", path.display()),
                        Err(_) => println!("# pod {pod} has no generated firewall"),
                    }
                    let out = pod::nft_output(&st.netns, &["list", "ruleset"])?;
                    println!(
                        "# loaded in the pod namespace:\n{}",
                        String::from_utf8_lossy(&out.stdout)
                    );
                    continue;
                }
                let out = pod::nft_output(&st.netns, &["list", "table", "inet", "gantry"])?;
                if !out.status.success() {
                    println!("pod {pod}: no firewall loaded; all traffic is allowed");
                    continue;
                }
                let chains = gantry::firewall::parse_listing(&String::from_utf8_lossy(&out.stdout));
                println!("pod {pod}");
                print!("{}", gantry::firewall::format_chains(&chains));
            }
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
    let containers = o.containers().await?;
    let mut rows = vec![
        ["SERVICE", "POD", "RUNTIME", "STATE", "CONTAINER"]
            .map(String::from)
            .to_vec(),
    ];
    for svc in o.project.services.values() {
        let found = containers
            .iter()
            .find(|(e, c)| c.service == svc.name && e.kind == svc.runtime && !c.oneoff);
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
        if c.oneoff {
            rows.push(vec![
                c.service.clone(),
                c.pod.clone(),
                e.kind.to_string(),
                format!("{} (run)", c.status),
                c.name.clone(),
            ]);
        } else if !o.project.services.contains_key(&c.service) {
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

/// `-e KEY` takes the value from this environment, as the Docker CLI
/// does; a key that is not set is dropped.
fn expand_env(env: Vec<String>) -> Vec<String> {
    env.into_iter()
        .filter_map(|e| {
            if e.contains('=') {
                Some(e)
            } else {
                std::env::var(&e).ok().map(|v| format!("{e}={v}"))
            }
        })
        .collect()
}

/// Projects with a saved snapshot under the run directory.
fn list_projects() {
    let mut rows = vec![
        ["NAME", "PODS", "SERVICES", "DIRECTORY"]
            .map(String::from)
            .to_vec(),
    ];
    let Ok(rd) = std::fs::read_dir(state::run_dir()) else {
        print_table(&rows);
        return;
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().join("project.json").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    for name in names {
        let Ok(Some(p)) = gantry::project::Project::load_snapshot(&name) else {
            continue;
        };
        let up = state::list_pods(&name)
            .iter()
            .filter(|pod| {
                state::PodState::load(&name, pod)
                    .ok()
                    .flatten()
                    .is_some_and(|s| pod::ns::is_pinned(&s.netns))
            })
            .count();
        rows.push(vec![
            name.clone(),
            format!("{up}/{}", p.pods.len()),
            p.services.len().to_string(),
            p.dir.display().to_string(),
        ]);
    }
    print_table(&rows);
}

fn print_table(rows: &[Vec<String>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| match widths.get(i) {
                Some(w) if i + 1 < r.len() => format!("{c:<w$}"),
                _ => c.clone(),
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
    use futures_util::StreamExt;
    use gantry::engine::LogOutput;
    let width = services.iter().map(String::len).max().unwrap_or(0);
    let mut tasks = Vec::new();
    for name in services {
        let svc = o
            .project
            .services
            .get(name)
            .ok_or_else(|| Error::config(format!("no such service {name:?}")))?;
        let mut stream = o
            .engine(svc.runtime)
            .logs(&svc.container_name, follow, tail.as_deref(), timestamps)
            .await?;
        let prefix = format!("{name:<width$} | ");
        tasks.push(tokio::spawn(async move {
            // Chunks need not end at line breaks; keep the partial line of
            // each stream until it is complete.
            let mut out_buf: Vec<u8> = Vec::new();
            let mut err_buf: Vec<u8> = Vec::new();
            while let Some(Ok(chunk)) = stream.next().await {
                let (buf, bytes) = match chunk {
                    LogOutput::StdErr { message } => (&mut err_buf, message),
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        (&mut out_buf, message)
                    }
                    LogOutput::StdIn { .. } => continue,
                };
                buf.extend_from_slice(&bytes);
                while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).collect();
                    let text = String::from_utf8_lossy(&line);
                    println!("{prefix}{}", text.trim_end_matches(['\n', '\r']));
                }
            }
            for buf in [out_buf, err_buf] {
                if !buf.is_empty() {
                    println!("{prefix}{}", String::from_utf8_lossy(&buf));
                }
            }
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
            .ctx(|| "locate gantry".into())?
            .with_file_name("gantry-oci"),
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
    runtimes[gantry::engine::DOCKER_RUNTIME] =
        serde_json::json!({ "path": oci.display().to_string() });
    if let Some(dir) = daemon_json.parent() {
        state::ensure_dir(dir)?;
    }
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| Error::State(e.to_string()))?;
    gantry::util::write_atomic(daemon_json, format!("{text}\n").as_bytes())?;
    println!(
        "Registered runtime {:?} -> {} in {}",
        gantry::engine::DOCKER_RUNTIME,
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
        println!("Docker is ready for gantry pods");
    }
    Ok(())
}
