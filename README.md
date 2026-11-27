# Gantry

Gantry is a replacement for Docker Compose that adds pods, and runs those
pods on Docker, with kiri also supported. You
keep writing Compose files. Gantry groups services into pods, puts each pod
straight onto your LAN with macvlan or ipvlan, applies an nftables firewall
inside the pod, and starts the pod's containers with the runtime each
service asks for.

## Why Gantry exists

Compose is the format most people already know, but it has no idea of a
pod. Every service gets its own network namespace, and putting a group of
containers on the LAN with one address, one hostname and one firewall means
fighting the bridge network. Kubernetes has pods, but it is far more than
a single host needs.

Gantry fills that gap:

* **Pods in plain Compose.** Services that belong together share one
  network, UTS and IPC namespace and talk over `127.0.0.1`, as in a
  Kubernetes pod.
* **Docker first, kiri too, even inside one pod.** Docker is the primary
  engine. kiri containers can sit beside Docker ones in the same pod; pick
  the runtime per project or per service.
* **First-class LAN networking.** Each pod gets its own address on a real
  network (static or DHCP, IPv4 and IPv6) and its own firewall, so it
  behaves like a small host rather than a set of published ports.
* **Self-healing.** A service with `health_on_failure: restart` is
  restarted when its healthcheck keeps failing, which engines do not do on
  their own.
* **One small binary.** No cluster. systemd brings projects back at boot,
  and one small watcher (`gantry supervise`) heals unhealthy services.

There is no infra or pause container. Gantry pins the namespaces the same
way `ip netns add` does, and every container joins them by path.

```
              LAN (vlan4, 192.0.2.216/29)
                |                        |
       macvlan 192.0.2.218       macvlan 192.0.2.219
                |                        |
        +-------+-----------------------+---------+-----------+
        | pod web  (netns, uts, ipc,    | pod data            |
        |          nftables)            |                     |
        |  nginx (docker)  <-127.0.0.1->|  redis (docker)     |
        |  sidecar (kiri)               |                     |
        +-------------------------------+---------------------+
```

## Status

Version 0.1.1, rapid development. Tested on Debian 13 (kernel 6.12) with
Docker 29.9 and kiri 0.2.0. Rootful only. Podman support was removed in
0.1.0.

## Install

Gantry needs Linux, root, `nft` (nftables) for pod firewalls, and at least
one engine.

From the INVITE Networks apt repository (Debian 12+, Ubuntu 22.04+,
amd64):

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://invite-networks.github.io/apt/invite-networks.asc |
  sudo tee /etc/apt/keyrings/invite-networks.asc >/dev/null
sudo tee /etc/apt/sources.list.d/invite-networks.sources >/dev/null <<'EOF'
Types: deb
URIs: https://invite-networks.github.io/apt
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/invite-networks.asc
EOF
sudo apt update && sudo apt install gantry
```

Each [release](https://github.com/invite-networks/gantry/releases) also
carries the `.deb` files with build provenance, which
`gh attestation verify <file>.deb --repo invite-networks/gantry` checks.

From source:

```sh
cargo build --release
sudo install -m 755 target/release/gantry target/release/gantry-oci /usr/local/bin/
```

Then, once per host, for each engine you use:

```sh
sudo gantry setup docker      # registers the gantry-oci runtime shim with dockerd
```

For kiri, see `contrib/kiri`.

## A first pod

```yaml
# gantry.yaml
name: demo

pod:                               # the only pod, named "demo" after the project
  network:
    - parent: eth0                   # host interface
      address: 192.168.10.50/24      # gateway defaults to 192.168.10.1

services:
  nginx:
    image: nginx:alpine
  app:
    build: ./app
    health_on_failure: restart     # restart it when its healthcheck keeps failing
```

With one pod there is nothing to name: services join it. The pod gets the default
firewall: any ICMP is accepted, everything else from `192.168.10.0/24` is
rejected, and other traffic is left to your router's firewall. Add `input`
rules to let neighbors in, or `defaults: false` to write the whole policy
yourself. See [the file format](docs/compose-spec.md).

```sh
sudo gantry up -d
sudo gantry ps
sudo gantry pod ls
sudo gantry logs -f app
sudo gantry down
```

`nginx` and `app` share the pod: one hostname, one address on the LAN, and
they talk over loopback. Other machines reach the pod at 192.168.10.50.

## Commands

| Command | What it does |
|---|---|
| `up [-d] [--wait] [--build] [--force-recreate] [--pull P] [svc...]` | Build or pull images, create pods, then create and start services. Services in the same dependency level start in parallel. Without `-d` it follows logs and stops services on Ctrl-C. |
| `down [-v] [-t SECS]` | Remove containers (in parallel, reverse dependency order), then pods (namespaces, links, DHCP leases). `-v` also removes named volumes. |
| `run [--rm] [-T] [--no-deps] [-e K=V] [-u U] [-w DIR] [--entrypoint E] svc [cmd...]` | Run a one-off container of a service in its pod, attached. Exits with the container's exit code. |
| `exec [-T] [-u U] [-w DIR] [-e K=V] svc cmd...` | Run a command in a running service. |
| `ps` | Services, their pod, runtime and state. |
| `logs [-f] [--tail N] [-t] [svc...]` | Logs, prefixed by service. `-f` keeps following through restarts and recreates. |
| `start`, `stop`, `restart [svc...]` | Start or stop services; pods stay up. |
| `kill [-s SIG] [svc...]`, `rm [-s] [svc...]` | Signal services; remove stopped service containers. |
| `top [svc...]` | Processes in each service. |
| `stats [--no-stream] [--json] [--interval N] [svc...]` | CPU (with the limit and how often it throttles), memory against its limit, processes and disk I/O per container, summed per pod, plus each pod's network traffic. Live until Ctrl-C. |
| `build [svc...]`, `pull [svc...]` | Build or pull images. Pulls run in parallel, one line per image with its download progress on a terminal, ending in `updated old -> new`, `up to date` or `pulled`. |
| `config [--services\|--pods\|--json]` | Validate and print the normalized project. Values that look like secrets print as `<redacted>`. |
| `data move [--dry-run] [--passes N] [--no-xattrs] [--force] [svc...]` | Move data to the paths the project file now names: live rsync passes, a short stop for the final pass and a checksum check, then the services are recreated on the new path. The old data is kept. |
| `ls` | Projects with pods on this host. |
| `pod ls`, `pod inspect POD`, `pod exec POD cmd...` | Pods, their addresses, state and DHCP leases; run a host command (`ip`, `tcpdump`, `nft`) in a pod's network namespace. |
| `firewall show [POD] [--raw]`, `firewall reload` | Every pod's loaded rules with hit counts (`--raw`: the generated ruleset and the nft listing); re-render hosts files and firewalls. |
| `plugin ls` | Installed plugins, their hooks, and whether the project uses them. |
| `setup docker` | One time host setup for Docker. |
| `supervise` | Watch every engine and act on unhealthy services that set `health_on_failure`. Runs as `gantry-supervise.service`. |
| `version`, `completion SHELL` | Versions of Gantry and each engine, and the features this build supports; shell completions. |

Global flags: `-f FILE` (repeatable, before the command, as with
`docker compose -f FILE logs -f`), `-p NAME`, `--project-directory`,
`--env-file`, `--profile`, `--runtime docker|kiri`, `--log-level`.

`up` without `-d` shows what every service prints from the moment `up`
starts, its startup output included, then keeps following until Ctrl-C.

## How it works

1. **Pod namespaces.** For each pod Gantry creates a network namespace at
   `/run/netns/gt-<project>-<pod>` (so `ip netns` sees it), plus UTS and IPC
   namespaces under `/run/gantry/<project>/pods/<pod>/`. It sets the hostname
   inside the UTS namespace.
2. **Links.** For every entry in the pod's `network` list it creates a
   macvlan or ipvlan link on the parent directly inside the pod namespace, renames it to `eth0`, `eth1`,
   ..., assigns static addresses and the default route, and sends
   gratuitous ARP. DHCP networks get a small agent process
   (`gantry internal dhcp-agent`) that lives in the pod namespace, holds the
   lease, renews it and releases it on `down`. When a lease changes address,
   the agent rewrites every pod's hosts file and firewall. Under systemd each
   agent runs in its own `gantry-dhcp-*.scope`, so it keeps running when
   whatever started the pod (a shell, `gantry-boot.service`, gantry-agent)
   stops or restarts.
3. **Shared files.** `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname`
   are written to the pod directory and bind mounted into every container.
4. **Firewall.** The default policy and the pod's rules are rendered into
   an `inet gantry` table and loaded with `nft` inside the pod namespace.
5. **Containers.** Each service is created through its engine's API and
   joined to the pod's namespaces by path. Labels (`io.gantry.*`) record the
   project, service, pod instance and a config hash, so a second `up` only
   touches what changed.

### Engines

Gantry talks to every engine through the Docker Engine API over its Unix
socket, using one client (`bollard`). No engine CLI is involved in normal
operation:

| Runtime | Socket | How containers join the pod |
|---|---|---|
| Docker | `/var/run/docker.sock` | `gantry-oci` runtime shim (below) |
| kiri | `/run/kiri/docker.sock` | `ns:<path>` modes, with the patches in `contrib/kiri` |

Image builds (`build:`) are the one exception: they run through
`docker build`, which brings BuildKit, `.dockerignore` and
build secrets. Registry credentials for pulls come from the same files
`docker login` writes, including credential helpers.

### Docker and the runtime shim

The Docker API cannot join a namespace by path. Gantry creates Docker
containers with runtime `gantry` and network `none`, plus annotations naming
the pod's namespaces. Docker passes annotations into the OCI spec;
`gantry-oci` rewrites the namespace paths in `config.json` (and drops the
hostname when the UTS namespace is shared) and execs `runc`. Set
`GANTRY_OCI_RUNTIME` in dockerd's environment to hand off to another
runtime (crun, for example). The shim only accepts namespace paths under
`/run/netns/` and `/run/gantry/`. Docker restarts keep working: Docker runs
the shim again and the container rejoins the pod.

### Performance

Measured on the test host with 8 services in 2 pods (4 Docker, 4 Podman, in
a release that still supported Podman):

| Operation | CLI-driven prototype | Gantry (API) |
|---|---|---|
| `up` from scratch | 2.3 s | 0.45 to 0.7 s |
| `up` with nothing to change | 820 ms | 80 ms |
| `ps` | 128 ms | 45 ms |
| `down` | 1.35 s | 0.35 to 0.6 s |

## Self-healing

Docker and kiri run healthchecks and mark a failing container `unhealthy`,
but neither restarts it: restart policies only cover containers that exit.
A service opts in to healing:

```yaml
services:
  app:
    image: example/app:1
    healthcheck:
      test: [CMD, wget, -qO-, http://127.0.0.1:8080/health]
      interval: 30s
      retries: 3
    health_on_failure: restart   # none (default), restart or stop
```

`gantry-supervise.service` (installed and enabled by the package) runs
`gantry supervise`, which checks every engine every 10 seconds and acts on
running containers that are `unhealthy`:

* `restart` restarts the container. The first restart is immediate, then
  the wait doubles from 30 seconds up to 5 minutes. After 5 restarts in 30
  minutes it gives up and leaves the container running until it turns
  healthy again or is recreated.
* `stop` stops it and leaves it stopped.

It never touches a stopped container, skips a project while a `gantry`
command (including boot) is working on it, and logs every action to the
journal (`journalctl -u gantry-supervise`). One watcher runs per host;
gantry-agent runs the same watcher and disables this unit when it takes
over.

## Running at boot

Pods live under `/run`, so they are gone after a reboot, and engine
restart policies cannot bring containers back on their own. One unit,
`gantry-boot.service` (enabled by the package), starts every registered
project at boot and stops them at shutdown.

`gantry up -d` on an installed project registers it, and `gantry down`
removes it. An installed project is the directory `<name>` in one of the
parents listed in `/etc/gantry/project-dirs`, by default `/etc/gantry/` or
`/mnt/*/`, so it can live next to its data.

Boot starts pods in `priority` order across every project on the host,
lowest first (default 100), and waits for each level to be running, or
healthy where a service has a healthcheck, before the next. Inside a pod,
service `priority` orders the services the same way. Equal priorities start
together.

```yaml
pods:
  dns:
    priority: 10        # before every pod at the default 100
services:
  db: { image: postgres:17, pod: app, priority: 10 }   # before app's others
```

```sh
sudo gantry --installed myproject up -d   # starts it and registers it
sudo gantry boot ls                       # the order boot will use
```

At boot there is no shell, so put every value a project needs in its
`.env`. A project registered by `gantry up` is loaded with the boot
service's environment ahead of `.env`, as before. A library caller can
register it with `boot::register_with(dir, &RegisterOptions { isolated_env:
true })`, and boot then loads it from its env files only, the way
`LoadOptions::isolated_env` does. `gantry up` never undoes that. The `ENV`
column of `boot ls` shows which.

## Remote control

Gantry runs on the host it manages. To drive it from elsewhere, the package
sets up a `gantry-remote` account that can only operate installed projects over
SSH, once you add a key to
`/etc/gantry/remote/authorized_keys`:

```sh
ssh gantry-remote@host myproject up --wait
ssh gantry-remote@host myproject logs --tail 100 web
```

See [docs/runbooks/remote.md](docs/runbooks/remote.md).

## Things to know

* **The host cannot reach its own macvlan pods** through the parent
  interface. That is how macvlan works. Reach them from another machine or
  through the router, or use a parent the host does not use itself.
* A parent interface carries macvlan or ipvlan links, not both.
* ipvlan links share the parent's MAC address, so DHCP servers must key
  leases on the client identifier (Gantry always sends one).
* Pods rely on the LAN for DNS. Without `dns:` a static pod uses the host's
  upstream servers (skipping loopback resolvers such as systemd-resolved's
  127.0.0.53, which a pod cannot reach).

## Environment

| Variable | Default | Use |
|---|---|---|
| `GANTRY_DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker API socket |
| `GANTRY_KIRI_HOST` | `unix:///run/kiri/docker.sock` | kiri API socket |
| `GANTRY_RUN_DIR` | `/run/gantry` | State directory |
| `GANTRY_NETNS_DIR` | `/run/netns` | Where network namespaces are pinned |
| `GANTRY_LOG` | `info` | Log level |
| `GANTRY_OCI_RUNTIME` | `runc` | Runtime the shim hands off to (set in dockerd's environment) |

## Development

```sh
cargo test                                         # unit tests (Linux)
GANTRY_DEV_HOST=user@host scripts/remote.sh cargo test   # from a Mac
cargo test --lib --no-default-features             # compose loader and checker only, any OS
sudo tests/integration/run.sh                      # live tests; see the script header
scripts/package.sh                                 # Debian package
```

A program that only needs to read or check project files, such as a
server validating them off the host, depends on `gantry` with
`default-features = false`. That leaves out the runtime (namespaces,
netlink, engines, the CLI), and the result builds on macOS. It provides
`compose::load` and `compose::check`.

See [docs/compose-spec.md](docs/compose-spec.md) for the file format,
and [docs/](docs/README.md) for requirements, design, test plan and runbooks.

Clones made before 2026-11-19 still hold the earlier test tags, and `git
fetch` will not replace their old `v0.1.0`. Refresh them once:

```sh
git tag -d $(git tag -l) && git fetch --tags origin
```

## License

MIT
