# Gantry

Gantry is a replacement for Docker Compose that adds pods, and runs those
pods on whichever container runtime you choose: Docker, Podman or kiri. You
keep writing Compose files. Gantry groups services into pods, puts each pod
straight onto your LAN with macvlan or ipvlan, applies an nftables firewall
inside the pod, and starts the pod's containers with the runtime each
service asks for.

## Why Gantry exists

Compose is the format most people already know, but it has no idea of a
pod. Every service gets its own network namespace, and putting a group of
containers on the LAN with one address, one hostname and one firewall means
fighting the bridge network. Kubernetes and Podman have pods, but
Kubernetes is far more than a single host needs, and Podman pods only work
with Podman.

Gantry fills that gap:

* **Pods in plain Compose.** Services that belong together share one
  network, UTS and IPC namespace and talk over `127.0.0.1`, as in a
  Kubernetes pod.
* **Any runtime, even inside one pod.** Docker, Podman and kiri containers
  can sit side by side in the same pod. Pick the runtime per project or per
  service, and switch without rewriting the file.
* **First-class LAN networking.** Each pod gets its own address on a real
  network (static or DHCP, IPv4 and IPv6) and its own firewall, so it
  behaves like a small host rather than a set of published ports.
* **One small binary.** No daemon of its own, no cluster, and systemd
  brings projects back at boot.

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
        |  nginx (docker)  <-127.0.0.1->|  redis (podman)     |
        |  sidecar (podman)             |                     |
        +-------------------------------+---------------------+
```

## Status

Version 0.1.0, rapid development. Tested on Debian 13 (kernel 6.12) with
Docker 29.9, Podman 5.4 and kiri 0.2.0. Rootful only.

## Install

Gantry needs Linux, root, `nft` (nftables) for pod firewalls, and at least
one engine.

From the INVITE Networks apt repository (Debian 12+, Ubuntu 22.04+, amd64
and arm64):

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
sudo gantry setup podman      # enables Podman's API socket (podman.socket)
```

For kiri, see `contrib/kiri`.

## A first pod

```yaml
# gantry.yaml
name: demo

networks:
  lan:
    driver: macvlan
    driver_opts: { parent: eth0 }
    ipam:
      config:
        - subnet: 192.168.10.0/24
          gateway: 192.168.10.1

pods:
  web:
    networks:
      lan: { ipv4_address: 192.168.10.50 }
    firewall:
      input:
        - { proto: tcp, ports: [80, 443] }

services:
  nginx:
    image: nginx:alpine
    pod: web
  app:
    build: ./app
    pod: web
    runtime: podman
```

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
| `logs [-f] [--tail N] [-t] [svc...]` | Logs, prefixed by service. |
| `start`, `stop`, `restart [svc...]` | Start or stop services; pods stay up. |
| `kill [-s SIG] [svc...]`, `rm [-s] [svc...]` | Signal services; remove stopped service containers. |
| `top [svc...]` | Processes in each service. |
| `build [svc...]`, `pull [svc...]` | Build or pull images. Pulls run in parallel. |
| `config [--services\|--pods\|--json]` | Validate and print the normalized project. |
| `ls` | Projects with pods on this host. |
| `pod ls`, `pod inspect POD`, `pod exec POD cmd...` | Pods, their addresses, state and DHCP leases; run a host command (`ip`, `tcpdump`, `nft`) in a pod's network namespace. |
| `firewall show POD`, `firewall reload` | The generated ruleset and what is loaded; re-render hosts files and firewalls. |
| `setup docker`, `setup podman` | One time host setup per engine. |
| `version`, `completion SHELL` | Versions of Gantry and each engine; shell completions. |

Global flags: `-f FILE` (repeatable), `-p NAME`, `--project-directory`,
`--env-file`, `--profile`, `--runtime docker|podman|kiri`, `--log-level`.

## How it works

1. **Pod namespaces.** For each pod Gantry creates a network namespace at
   `/run/netns/gt-<project>-<pod>` (so `ip netns` sees it), plus UTS and IPC
   namespaces under `/run/gantry/<project>/pods/<pod>/`. It sets the hostname
   inside the UTS namespace.
2. **Links.** For every network it creates a macvlan or ipvlan link on the
   parent directly inside the pod namespace, renames it to `eth0`, `eth1`,
   ..., assigns static addresses and the default route, and sends
   gratuitous ARP. DHCP networks get a small agent process
   (`gantry internal dhcp-agent`) that lives in the pod namespace, holds the
   lease, renews it and releases it on `down`. When a lease changes address,
   the agent rewrites every pod's hosts file and firewall.
3. **Shared files.** `/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname`
   are written to the pod directory and bind mounted into every container.
4. **Firewall.** The pod's rules are rendered into an `inet gantry` table
   and loaded with `nft` inside the pod namespace.
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
| Podman | `/run/podman/podman.sock` (Docker-compatible API) | `ns:<path>` network, UTS and IPC modes |
| kiri | `/run/kiri/docker.sock` | `ns:<path>` modes, with the patches in `contrib/kiri` |

If Podman's socket is not active, Gantry starts `podman.socket`, or as a
last resort a private `podman system service` that exits when idle.

Image builds (`build:`) are the one exception: they run through
`docker build` or `podman build`, which brings BuildKit, `.dockerignore` and
build secrets. Registry credentials for pulls come from the same files
`docker login` and `podman login` write, including credential helpers.

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

Measured on the test host with 8 services in 2 pods (4 Docker, 4 Podman):

| Operation | CLI-driven prototype | Gantry (API) |
|---|---|---|
| `up` from scratch | 2.3 s | 0.45 to 0.7 s |
| `up` with nothing to change | 820 ms | 80 ms |
| `ps` | 128 ms | 45 ms |
| `down` | 1.35 s | 0.35 to 0.6 s |

## Running at boot

Pods live under `/run`, so they are gone after a reboot. The unit
`gantry@.service` (installed by the package, or copy it from
`contrib/systemd/`) runs `gantry up -d --wait` for `/etc/gantry/<name>/` at
boot and `gantry down` at shutdown:

```sh
sudo systemctl enable --now gantry@myproject
```

## Remote control

Gantry runs on the host it manages. To drive it from elsewhere, the package
sets up a `gantry` account that can only operate projects in
`/etc/gantry/<name>/` over SSH, once you add a key to
`/etc/gantry/remote/authorized_keys`:

```sh
ssh gantry@host myproject up --wait
ssh gantry@host myproject logs --tail 100 web
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
| `GANTRY_PODMAN_HOST` | `unix:///run/podman/podman.sock` | Podman API socket |
| `GANTRY_KIRI_HOST` | `unix:///run/kiri/docker.sock` | kiri API socket |
| `GANTRY_RUN_DIR` | `/run/gantry` | State directory |
| `GANTRY_NETNS_DIR` | `/run/netns` | Where network namespaces are pinned |
| `GANTRY_LOG` | `info` | Log level |
| `GANTRY_OCI_RUNTIME` | `runc` | Runtime the shim hands off to (set in dockerd's environment) |

## Development

```sh
cargo test                                         # unit tests (Linux)
GANTRY_DEV_HOST=user@host scripts/remote.sh cargo test   # from a Mac
sudo tests/integration/run.sh                      # live tests; see the script header
scripts/package.sh                                 # Debian package
```

See [docs/compose-spec.md](docs/compose-spec.md) for the file format,
and [docs/](docs/README.md) for requirements, design, test plan and runbooks.

## License

MIT
